//! Pauses media playback (e.g. a YouTube video) while recording and resumes
//! it afterwards. Based on the approach in cjpais/Handy#1028; only Windows has
//! a backend, other platforms do nothing.

use crate::managers::audio::AudioRecordingManager;
use crate::settings::get_settings;
use log::{debug, info, warn};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager};

/// A media session Handy paused: its source app id plus the current title, so
/// that another tab of the same browser that the user paused is not resumed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PausedSession {
    pub source_app_id: String,
    pub title: String,
}

trait MediaControlBackend: Send + Sync {
    fn pause_all(&self) -> Result<Vec<PausedSession>, String>;
    fn resume(&self, sessions: &[PausedSession]) -> Result<(), String>;
}

#[derive(Default)]
struct SessionState {
    generation: u64,
    pause_in_flight: bool,
    paused: Vec<PausedSession>,
    /// Set when the recording ended while a pause was still in flight, so the
    /// pause thread resumes whatever it paused once its result arrives.
    resume_when_pause_lands: bool,
}

pub struct MediaControlManager {
    state: Mutex<SessionState>,
    backend: Arc<dyn MediaControlBackend>,
}

impl MediaControlManager {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(SessionState::default()),
            backend: Arc::new(PlatformBackend),
        }
    }

    #[cfg(test)]
    fn with_backend(backend: Arc<dyn MediaControlBackend>) -> Self {
        Self {
            state: Mutex::new(SessionState::default()),
            backend,
        }
    }

    /// Pauses everything that is playing, if enabled and still recording.
    pub fn pause_for_recording(&self, app: &AppHandle) {
        let enabled = get_settings(app).pause_while_recording;
        let recording_active = app
            .try_state::<Arc<AudioRecordingManager>>()
            .map(|rm| rm.is_recording())
            .unwrap_or(false);
        self.pause_inner(enabled, recording_active);
    }

    /// Resumes whatever Handy paused for the recording that just ended.
    pub fn resume_after_recording(&self) {
        self.resume_inner();
    }

    fn pause_inner(&self, enabled: bool, recording_active: bool) {
        if !enabled || !recording_active {
            return;
        }

        let generation = {
            let mut state = self.state.lock().unwrap();
            if state.pause_in_flight || !state.paused.is_empty() {
                return;
            }
            state.generation = state.generation.wrapping_add(1);
            state.pause_in_flight = true;
            state.resume_when_pause_lands = false;
            state.generation
        };

        let result = self.backend.pause_all();
        let mut state = self.state.lock().unwrap();

        if state.generation != generation {
            // The recording ended while we were pausing.
            let resume = state.resume_when_pause_lands;
            state.resume_when_pause_lands = false;
            drop(state);
            if let (true, Ok(sessions)) = (resume, result) {
                if !sessions.is_empty() {
                    if let Err(err) = self.backend.resume(&sessions) {
                        warn!("Failed to resume media after a stale pause: {err}");
                    }
                }
            }
            return;
        }

        state.pause_in_flight = false;
        match result {
            Ok(sessions) if sessions.is_empty() => debug!("No media playing to pause"),
            Ok(sessions) => {
                info!("Paused {} media session(s) for recording", sessions.len());
                state.paused = sessions;
            }
            Err(err) => warn!("Failed to pause media for recording: {err}"),
        }
    }

    fn resume_inner(&self) {
        let paused = {
            let mut state = self.state.lock().unwrap();
            state.generation = state.generation.wrapping_add(1);
            state.resume_when_pause_lands = state.pause_in_flight;
            state.pause_in_flight = false;
            std::mem::take(&mut state.paused)
        };

        if paused.is_empty() {
            return;
        }

        match self.backend.resume(&paused) {
            Ok(()) => info!("Resumed {} media session(s) after recording", paused.len()),
            Err(err) => warn!("Failed to resume media after recording: {err}"),
        }
    }
}

impl Default for MediaControlManager {
    fn default() -> Self {
        Self::new()
    }
}

struct PlatformBackend;

#[cfg(target_os = "windows")]
impl MediaControlBackend for PlatformBackend {
    fn pause_all(&self) -> Result<Vec<PausedSession>, String> {
        windows_backend::pause_all_playing()
    }

    fn resume(&self, sessions: &[PausedSession]) -> Result<(), String> {
        windows_backend::resume(sessions)
    }
}

#[cfg(not(target_os = "windows"))]
impl MediaControlBackend for PlatformBackend {
    fn pause_all(&self) -> Result<Vec<PausedSession>, String> {
        Ok(Vec::new())
    }

    fn resume(&self, _sessions: &[PausedSession]) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(target_os = "windows")]
mod windows_backend {
    use super::PausedSession;
    use windows::core::HRESULT;
    use windows::Media::Control::{
        GlobalSystemMediaTransportControlsSession as Session,
        GlobalSystemMediaTransportControlsSessionManager as SessionManager,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
    };
    use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
    use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};

    struct WinRtGuard {
        uninitialize: bool,
    }

    impl WinRtGuard {
        fn initialize() -> Result<Self, String> {
            match unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
                Ok(()) => Ok(Self { uninitialize: true }),
                Err(err) if err.code() == RPC_E_CHANGED_MODE => Ok(Self {
                    uninitialize: false,
                }),
                Err(err) => Err(format!("Failed to initialize WinRT: {err}")),
            }
        }
    }

    impl Drop for WinRtGuard {
        fn drop(&mut self) {
            if self.uninitialize {
                unsafe { RoUninitialize() };
            }
        }
    }

    pub fn pause_all_playing() -> Result<Vec<PausedSession>, String> {
        let _guard = WinRtGuard::initialize()?;
        let mut paused = Vec::new();

        for session in sessions()? {
            if status(&session) != Some(PlaybackStatus::Playing) {
                continue;
            }
            let pause_enabled = session
                .GetPlaybackInfo()
                .and_then(|info| info.Controls())
                .and_then(|controls| controls.IsPauseEnabled())
                .unwrap_or(false);
            if !pause_enabled {
                continue;
            }
            let Some(key) = key(&session) else {
                continue;
            };
            let did_pause = session
                .TryPauseAsync()
                .and_then(|op| op.get())
                .unwrap_or(false);
            if did_pause {
                paused.push(key);
            }
        }

        Ok(paused)
    }

    pub fn resume(paused: &[PausedSession]) -> Result<(), String> {
        let _guard = WinRtGuard::initialize()?;

        for session in sessions()? {
            // Only resume sessions that are still paused: one the user started
            // again in the meantime must not be touched.
            if status(&session) != Some(PlaybackStatus::Paused) {
                continue;
            }
            let Some(key) = key(&session) else {
                continue;
            };
            if !paused.contains(&key) {
                continue;
            }
            let _ = session.TryPlayAsync().and_then(|op| op.get());
        }

        Ok(())
    }

    fn sessions() -> Result<Vec<Session>, String> {
        let manager = SessionManager::RequestAsync()
            .and_then(|op| op.get())
            .map_err(|err| format!("Failed to get the Windows media session manager: {err}"))?;
        let list = match manager.GetSessions() {
            Ok(list) => list,
            Err(err) if session_missing(err.code()) => return Ok(Vec::new()),
            Err(err) => return Err(format!("Failed to list Windows media sessions: {err}")),
        };
        let count = list.Size().unwrap_or(0);
        Ok((0..count).filter_map(|i| list.GetAt(i).ok()).collect())
    }

    fn status(session: &Session) -> Option<PlaybackStatus> {
        session
            .GetPlaybackInfo()
            .and_then(|info| info.PlaybackStatus())
            .ok()
    }

    fn key(session: &Session) -> Option<PausedSession> {
        let source_app_id = session.SourceAppUserModelId().ok()?.to_string();
        let title = session
            .TryGetMediaPropertiesAsync()
            .and_then(|op| op.get())
            .and_then(|props| props.Title())
            .map(|title| title.to_string())
            .unwrap_or_default();
        Some(PausedSession {
            source_app_id,
            title,
        })
    }

    fn session_missing(code: HRESULT) -> bool {
        code == HRESULT(0x80070490u32 as i32) || code == HRESULT(0x80004005u32 as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct TestBackend {
        pause_calls: AtomicUsize,
        resume_calls: AtomicUsize,
        pause_result: Vec<PausedSession>,
        pause_delay: Option<Duration>,
    }

    impl TestBackend {
        fn new(pause_result: Vec<PausedSession>, pause_delay: Option<Duration>) -> Arc<Self> {
            Arc::new(Self {
                pause_calls: AtomicUsize::new(0),
                resume_calls: AtomicUsize::new(0),
                pause_result,
                pause_delay,
            })
        }
    }

    impl MediaControlBackend for TestBackend {
        fn pause_all(&self) -> Result<Vec<PausedSession>, String> {
            self.pause_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(delay) = self.pause_delay {
                std::thread::sleep(delay);
            }
            Ok(self.pause_result.clone())
        }

        fn resume(&self, _sessions: &[PausedSession]) -> Result<(), String> {
            self.resume_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn video() -> Vec<PausedSession> {
        vec![PausedSession {
            source_app_id: "chrome".into(),
            title: "Video".into(),
        }]
    }

    #[test]
    fn disabled_or_not_recording_does_nothing() {
        let backend = TestBackend::new(video(), None);
        let manager = MediaControlManager::with_backend(backend.clone());
        manager.pause_inner(false, true);
        manager.pause_inner(true, false);
        assert_eq!(backend.pause_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn pauses_once_and_resumes_once() {
        let backend = TestBackend::new(video(), None);
        let manager = MediaControlManager::with_backend(backend.clone());
        manager.pause_inner(true, true);
        manager.pause_inner(true, true);
        manager.resume_inner();
        manager.resume_inner();
        assert_eq!(backend.pause_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.resume_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn nothing_paused_means_nothing_resumed() {
        let backend = TestBackend::new(Vec::new(), None);
        let manager = MediaControlManager::with_backend(backend.clone());
        manager.pause_inner(true, true);
        manager.resume_inner();
        assert_eq!(backend.resume_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn pause_that_lands_after_stop_is_resumed() {
        let backend = TestBackend::new(video(), Some(Duration::from_millis(100)));
        let manager = Arc::new(MediaControlManager::with_backend(backend.clone()));
        let pausing = Arc::clone(&manager);
        let handle = std::thread::spawn(move || pausing.pause_inner(true, true));
        while backend.pause_calls.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        manager.resume_inner();
        handle.join().unwrap();
        manager.resume_inner();
        assert_eq!(backend.resume_calls.load(Ordering::SeqCst), 1);
    }
}
