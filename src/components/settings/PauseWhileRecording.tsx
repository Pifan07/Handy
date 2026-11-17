import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface PauseWhileRecordingToggleProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const PauseWhileRecording: React.FC<PauseWhileRecordingToggleProps> =
  React.memo(({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();

    const pauseEnabled = getSetting("pause_while_recording") ?? false;

    return (
      <ToggleSwitch
        checked={pauseEnabled}
        onChange={(enabled) => updateSetting("pause_while_recording", enabled)}
        isUpdating={isUpdating("pause_while_recording")}
        label={t("settings.debug.pauseWhileRecording.label")}
        description={t("settings.debug.pauseWhileRecording.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
    );
  });
