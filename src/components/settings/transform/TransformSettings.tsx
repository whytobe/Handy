import React, { useCallback, useEffect, useRef, useState } from "react";
import { Trans, useTranslation } from "react-i18next";
import { Mic, RefreshCcw, Trash2, X, Zap } from "lucide-react";
import {
  commands,
  type AppInfo,
  type LLMPrompt,
  type TransformAppRule,
  type TransformMode,
  type TransformSelectionBehavior,
} from "@/bindings";
import { useOsType } from "@/hooks/useOsType";
import { usePressAway } from "@/hooks/usePressAway";
import { useSettings } from "@/hooks/useSettings";
import { useShortcutCapture } from "@/hooks/useShortcutCapture";
import { formatKeyCombination } from "@/lib/utils/keyboard";
import {
  Dropdown,
  SettingContainer,
  SettingsGroup,
  Textarea,
  ToggleSwitch,
} from "@/components/ui";
import { Alert } from "@/components/ui/Alert";
import { Button } from "@/components/ui/Button";
import { Input } from "@/components/ui/Input";
import { ResetButton } from "@/components/ui/ResetButton";
import { Switch } from "@/components/ui/Switch";
import { ShortcutInput } from "../ShortcutInput";
import { PostProcessingSettingsApi } from "../PostProcessingSettingsApi";

const USE_POST_PROCESSING_PROMPT_VALUE = "__use_post_processing_prompt__";
const DEFAULT_COMMAND_TRANSFORM_PROMPT_ID = "default_command_transform";

const promptBindingId = (promptId: string) => `transform_prompt:${promptId}`;

const defaultSelectionBehaviorForPrompt = (
  promptId: string,
): TransformSelectionBehavior =>
  promptId === DEFAULT_COMMAND_TRANSFORM_PROMPT_ID
    ? "voice_command"
    : "instant";

const formatAppOptionLabel = (app: AppInfo): string =>
  app.bundle_id ? `${app.name} (${app.bundle_id})` : app.name;

interface PromptShortcutRowProps {
  prompt: LLMPrompt;
  currentBinding: string | null;
  selectionBehavior: TransformSelectionBehavior;
  disabled: boolean;
  onClear: (promptId: string) => Promise<void>;
  onChangeSelectionBehavior: (
    promptId: string,
    behavior: TransformSelectionBehavior,
  ) => Promise<void>;
}

const PromptShortcutRow: React.FC<PromptShortcutRowProps> = ({
  prompt,
  currentBinding,
  selectionBehavior,
  disabled,
  onClear,
  onChangeSelectionBehavior,
}) => {
  const { t } = useTranslation();
  const osType = useOsType();
  const { isUpdating } = useSettings();
  const bindingId = promptBindingId(prompt.id);
  const { capturing, partial, start, cancel } = useShortcutCapture(
    bindingId,
    osType,
    { escapeCancels: true },
  );
  const shortcutRef = useRef<HTMLDivElement | null>(null);
  usePressAway(capturing, shortcutRef, cancel);
  const [savingBehavior, setSavingBehavior] = useState(false);

  const isBusy = isUpdating(`binding_${bindingId}`);
  const isInstant = selectionBehavior === "instant";
  const behaviorTooltip = t(
    isInstant
      ? "settings.transform.prompts.shortcuts.behaviorInstantTooltip"
      : "settings.transform.prompts.shortcuts.behaviorVoiceTooltip",
  );

  const handleToggleBehavior = async () => {
    setSavingBehavior(true);
    try {
      await onChangeSelectionBehavior(
        prompt.id,
        isInstant ? "voice_command" : "instant",
      );
    } finally {
      setSavingBehavior(false);
    }
  };

  return (
    <div className="flex items-center justify-between gap-3 px-3 py-2.5">
      <div className="min-w-0 flex-1">
        <p className="text-sm font-medium truncate">{prompt.name}</p>
        <p className="text-xs text-ink-3 truncate">{prompt.prompt}</p>
      </div>

      <div className="flex items-center gap-1.5 shrink-0">
        <button
          type="button"
          onClick={() => void handleToggleBehavior()}
          disabled={savingBehavior}
          title={behaviorTooltip}
          aria-label={behaviorTooltip}
          className={`inline-flex items-center gap-1 px-2 py-1 text-xs font-medium rounded-md border whitespace-nowrap transition-colors cursor-pointer disabled:opacity-50 ${
            isInstant
              ? "bg-logo-primary/15 border-logo-primary/40 text-logo-primary hover:bg-logo-primary/25"
              : "bg-wash border-line-strong text-ink-2 hover:text-text hover:border-logo-primary"
          } ${disabled ? "opacity-75" : ""}`}
        >
          {isInstant ? (
            <Zap className="w-3 h-3 shrink-0" />
          ) : (
            <Mic className="w-3 h-3 shrink-0" />
          )}
          <span>
            {t(
              isInstant
                ? "settings.transform.prompts.shortcuts.behaviorInstant"
                : "settings.transform.prompts.shortcuts.behaviorVoice",
            )}
          </span>
        </button>

        {capturing ? (
          <div
            ref={shortcutRef}
            className="px-2 py-1 text-sm font-semibold border border-logo-primary bg-logo-primary/20 rounded-md"
          >
            {partial
              ? formatKeyCombination(partial, osType)
              : t("settings.general.shortcut.pressKeys")}
          </div>
        ) : currentBinding ? (
          <>
            <button
              type="button"
              onClick={start}
              disabled={isBusy}
              className={`px-2 py-1 text-sm font-semibold bg-wash border border-line-strong hover:bg-logo-primary/10 rounded-md cursor-pointer hover:border-logo-primary disabled:opacity-50 ${
                disabled ? "opacity-75" : ""
              }`}
            >
              {formatKeyCombination(currentBinding, osType)}
            </button>
            <Button
              variant="danger-ghost"
              size="sm"
              className="shrink-0"
              disabled={isBusy}
              onClick={() => void onClear(prompt.id)}
              aria-label={t(
                "settings.transform.prompts.shortcuts.clearShortcut",
                {
                  prompt: prompt.name,
                },
              )}
            >
              <X className="w-3.5 h-3.5" />
            </Button>
          </>
        ) : (
          <button
            type="button"
            onClick={start}
            disabled={isBusy}
            className={`px-2 py-1 text-xs font-medium text-ink-3 border border-dashed border-line-strong rounded-md hover:text-text hover:border-logo-primary hover:bg-logo-primary/10 cursor-pointer disabled:opacity-50 ${
              disabled ? "opacity-75" : ""
            }`}
          >
            {t("settings.transform.prompts.shortcuts.setShortcut")}
          </button>
        )}
      </div>
    </div>
  );
};

const TransformCoreSettings: React.FC = () => {
  const { t } = useTranslation();
  const osType = useOsType();
  const { getSetting, updateSetting, isUpdating } = useSettings();

  const transformEnabled = getSetting("transform_enabled") ?? false;
  const transformMode = (getSetting("transform_mode") ??
    "smart") as TransformMode;
  const clipboardFallback = getSetting("transform_clipboard_fallback") ?? true;

  const modeOptions = [
    {
      value: "smart",
      label: t("settings.transform.mode.options.smart"),
      description: t("settings.transform.mode.descriptions.smart"),
    },
    {
      value: "command",
      label: t("settings.transform.mode.options.command"),
      description: t("settings.transform.mode.descriptions.command"),
    },
    {
      value: "auto_style",
      label: t("settings.transform.mode.options.autoStyle"),
      description: t("settings.transform.mode.descriptions.autoStyle"),
    },
  ];

  const showClipboardFallback =
    osType === "macos" &&
    (transformMode === "smart" || transformMode === "command");

  return (
    <SettingsGroup title={t("settings.transform.title")}>
      {osType !== "macos" && osType !== "unknown" && (
        <Alert variant="info" contained>
          {t("settings.transform.macosOnlyNotice")}
        </Alert>
      )}

      <ToggleSwitch
        checked={transformEnabled}
        onChange={(enabled) => updateSetting("transform_enabled", enabled)}
        isUpdating={isUpdating("transform_enabled")}
        label={t("settings.transform.enable.label")}
        description={t("settings.transform.enable.description")}
        descriptionMode="tooltip"
        grouped={true}
      />

      <ShortcutInput
        shortcutId="transform_text"
        descriptionMode="tooltip"
        grouped={true}
      />

      <SettingContainer
        title={t("settings.transform.mode.title")}
        description={t("settings.transform.mode.description")}
        descriptionMode="tooltip"
        grouped={true}
      >
        <Dropdown
          options={modeOptions}
          menuClassName="right-0 w-80 max-w-[calc(100vw-2rem)]"
          selectedValue={transformMode}
          onSelect={(value) =>
            updateSetting("transform_mode", value as TransformMode)
          }
          disabled={isUpdating("transform_mode")}
        />
      </SettingContainer>

      {showClipboardFallback && (
        <ToggleSwitch
          checked={clipboardFallback}
          onChange={(enabled) =>
            updateSetting("transform_clipboard_fallback", enabled)
          }
          isUpdating={isUpdating("transform_clipboard_fallback")}
          label={t("settings.transform.clipboardFallback.label")}
          description={t("settings.transform.clipboardFallback.description")}
          descriptionMode="tooltip"
          grouped={true}
        />
      )}
    </SettingsGroup>
  );
};

const TransformPromptsSettings: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating, refreshSettings } =
    useSettings();
  const [isCreating, setIsCreating] = useState(false);
  const [draftName, setDraftName] = useState("");
  const [draftText, setDraftText] = useState("");

  const transformEnabled = getSetting("transform_enabled") ?? false;
  const transformMode = (getSetting("transform_mode") ??
    "smart") as TransformMode;
  const prompts = getSetting("transform_prompts") || [];
  const bindings = getSetting("bindings") || {};
  const selectionBehaviors =
    getSetting("transform_prompt_selection_behaviors") || {};
  const selectedPromptId = getSetting("transform_selected_prompt_id") || "";
  const defaultDictationPromptId =
    getSetting("transform_default_dictation_prompt_id") || null;
  const selectedPrompt =
    prompts.find((prompt) => prompt.id === selectedPromptId) || null;

  useEffect(() => {
    if (isCreating) return;

    if (selectedPrompt) {
      setDraftName(selectedPrompt.name);
      setDraftText(selectedPrompt.prompt);
    } else {
      setDraftName("");
      setDraftText("");
    }
  }, [
    isCreating,
    selectedPromptId,
    selectedPrompt?.name,
    selectedPrompt?.prompt,
  ]);

  const handlePromptSelect = (promptId: string | null) => {
    if (!promptId) return;
    void updateSetting("transform_selected_prompt_id", promptId);
    setIsCreating(false);
  };

  const handleCreatePrompt = async () => {
    if (!draftName.trim() || !draftText.trim()) return;

    try {
      const result = await commands.addTransformPrompt(
        draftName.trim(),
        draftText.trim(),
      );
      if (result.status === "ok") {
        await refreshSettings();
        await updateSetting("transform_selected_prompt_id", result.data.id);
        setIsCreating(false);
      }
    } catch (error) {
      console.error("Failed to create transform prompt:", error);
    }
  };

  const handleUpdatePrompt = async () => {
    if (!selectedPromptId || !draftName.trim() || !draftText.trim()) return;

    try {
      await commands.updateTransformPrompt(
        selectedPromptId,
        draftName.trim(),
        draftText.trim(),
      );
      await refreshSettings();
    } catch (error) {
      console.error("Failed to update transform prompt:", error);
    }
  };

  const handleDeletePrompt = async (promptId: string) => {
    if (!promptId) return;

    try {
      await commands.deleteTransformPrompt(promptId);
      await refreshSettings();
      setIsCreating(false);
    } catch (error) {
      console.error("Failed to delete transform prompt:", error);
    }
  };

  const handleClearPromptShortcut = async (promptId: string) => {
    try {
      await commands.clearTransformPromptBinding(promptId);
      await refreshSettings();
    } catch (error) {
      console.error("Failed to clear prompt shortcut:", error);
    }
  };

  const handleChangeSelectionBehavior = async (
    promptId: string,
    behavior: TransformSelectionBehavior,
  ) => {
    try {
      const result = await commands.setTransformPromptSelectionBehavior(
        promptId,
        behavior,
      );
      if (result.status === "ok") {
        await refreshSettings();
      } else {
        console.error("Failed to set prompt selection behavior:", result.error);
      }
    } catch (error) {
      console.error("Failed to set prompt selection behavior:", error);
    }
  };

  const handleCancelCreate = () => {
    setIsCreating(false);
    if (selectedPrompt) {
      setDraftName(selectedPrompt.name);
      setDraftText(selectedPrompt.prompt);
    } else {
      setDraftName("");
      setDraftText("");
    }
  };

  const handleStartCreate = () => {
    setIsCreating(true);
    setDraftName("");
    setDraftText("");
  };

  const hasPrompts = prompts.length > 0;
  const isDirty =
    !!selectedPrompt &&
    (draftName.trim() !== selectedPrompt.name ||
      draftText.trim() !== selectedPrompt.prompt.trim());

  const showFallbackDictationSelector =
    transformMode === "smart" || transformMode === "auto_style";

  const fallbackOptions = [
    {
      value: USE_POST_PROCESSING_PROMPT_VALUE,
      label: t(
        "settings.transform.prompts.defaultDictationPrompt.usePostProcessing",
      ),
    },
    ...prompts.map((p) => ({
      value: p.id,
      label: p.name,
    })),
  ];

  return (
    <SettingsGroup title={t("settings.transform.prompts.title")}>
      <SettingContainer
        title={t("settings.transform.prompts.selectedPrompt.title")}
        description={t("settings.transform.prompts.selectedPrompt.description")}
        descriptionMode="tooltip"
        layout="stacked"
        grouped={true}
      >
        <div className="space-y-3">
          <div className="flex gap-2 min-w-0">
            <Dropdown
              selectedValue={selectedPromptId || null}
              options={prompts.map((p) => ({
                value: p.id,
                label: p.name,
              }))}
              onSelect={(value) => handlePromptSelect(value)}
              placeholder={
                prompts.length === 0
                  ? t("settings.transform.prompts.noPrompts")
                  : t("settings.transform.prompts.selectPrompt")
              }
              disabled={
                isUpdating("transform_selected_prompt_id") || isCreating
              }
              className="flex-1 min-w-0"
            />
            <Button
              onClick={handleStartCreate}
              variant="primary"
              size="md"
              disabled={isCreating}
              className="shrink-0"
            >
              {t("settings.transform.prompts.createNew")}
            </Button>
          </div>

          {!isCreating && hasPrompts && selectedPrompt && (
            <div className="space-y-3">
              <div className="space-y-2 flex flex-col">
                <label className="text-sm font-semibold">
                  {t("settings.transform.prompts.promptLabel")}
                </label>
                <Input
                  type="text"
                  value={draftName}
                  onChange={(e) => setDraftName(e.target.value)}
                  placeholder={t(
                    "settings.transform.prompts.promptLabelPlaceholder",
                  )}
                  variant="compact"
                />
              </div>

              <div className="space-y-2 flex flex-col">
                <label className="text-sm font-semibold">
                  {t("settings.transform.prompts.promptInstructions")}
                </label>
                <Textarea
                  value={draftText}
                  onChange={(e) => setDraftText(e.target.value)}
                  placeholder={t(
                    "settings.transform.prompts.promptInstructionsPlaceholder",
                  )}
                />
                <p className="text-xs text-ink-3">
                  <Trans
                    i18nKey="settings.transform.prompts.promptTip"
                    components={{ code: <code /> }}
                  />
                </p>
              </div>

              <div className="flex gap-2 pt-2">
                <Button
                  onClick={handleUpdatePrompt}
                  variant="primary"
                  size="md"
                  disabled={!draftName.trim() || !draftText.trim() || !isDirty}
                >
                  {t("settings.transform.prompts.updatePrompt")}
                </Button>
                <Button
                  onClick={() => handleDeletePrompt(selectedPromptId)}
                  variant="secondary"
                  size="md"
                  disabled={!selectedPromptId || prompts.length <= 1}
                >
                  {t("settings.transform.prompts.deletePrompt")}
                </Button>
              </div>
            </div>
          )}

          {!isCreating && !selectedPrompt && (
            <div className="p-3 bg-wash rounded-md border border-line">
              <p className="text-sm text-ink-3">
                {hasPrompts
                  ? t("settings.transform.prompts.selectToEdit")
                  : t("settings.transform.prompts.createFirst")}
              </p>
            </div>
          )}

          {isCreating && (
            <div className="space-y-3">
              <div className="space-y-2 flex flex-col">
                <label className="text-sm font-semibold text-text">
                  {t("settings.transform.prompts.promptLabel")}
                </label>
                <Input
                  type="text"
                  value={draftName}
                  onChange={(e) => setDraftName(e.target.value)}
                  placeholder={t(
                    "settings.transform.prompts.promptLabelPlaceholder",
                  )}
                  variant="compact"
                />
              </div>

              <div className="space-y-2 flex flex-col">
                <label className="text-sm font-semibold">
                  {t("settings.transform.prompts.promptInstructions")}
                </label>
                <Textarea
                  value={draftText}
                  onChange={(e) => setDraftText(e.target.value)}
                  placeholder={t(
                    "settings.transform.prompts.promptInstructionsPlaceholder",
                  )}
                />
                <p className="text-xs text-ink-3">
                  <Trans
                    i18nKey="settings.transform.prompts.promptTip"
                    components={{ code: <code /> }}
                  />
                </p>
              </div>

              <div className="flex gap-2 pt-2">
                <Button
                  onClick={handleCreatePrompt}
                  variant="primary"
                  size="md"
                  disabled={!draftName.trim() || !draftText.trim()}
                >
                  {t("settings.transform.prompts.createPrompt")}
                </Button>
                <Button
                  onClick={handleCancelCreate}
                  variant="secondary"
                  size="md"
                >
                  {t("settings.transform.prompts.cancel")}
                </Button>
              </div>
            </div>
          )}
        </div>
      </SettingContainer>

      <SettingContainer
        title={t("settings.transform.prompts.shortcuts.title")}
        description={t("settings.transform.prompts.shortcuts.description")}
        descriptionMode="tooltip"
        layout="stacked"
        grouped={true}
      >
        <div className="space-y-2.5">
          {!transformEnabled && (
            <p className="text-xs text-ink-3">
              {t("settings.transform.prompts.shortcuts.enableHint")}
            </p>
          )}
          <div className="divide-y divide-line border border-line rounded-md">
            {prompts.map((prompt) => (
              <PromptShortcutRow
                key={prompt.id}
                prompt={prompt}
                currentBinding={
                  bindings[promptBindingId(prompt.id)]?.current_binding || null
                }
                selectionBehavior={
                  selectionBehaviors[prompt.id] ??
                  defaultSelectionBehaviorForPrompt(prompt.id)
                }
                disabled={!transformEnabled}
                onClear={handleClearPromptShortcut}
                onChangeSelectionBehavior={handleChangeSelectionBehavior}
              />
            ))}
          </div>
        </div>
      </SettingContainer>

      {showFallbackDictationSelector && (
        <SettingContainer
          title={t("settings.transform.prompts.defaultDictationPrompt.title")}
          description={t(
            "settings.transform.prompts.defaultDictationPrompt.description",
          )}
          descriptionMode="tooltip"
          grouped={true}
        >
          <Dropdown
            options={fallbackOptions}
            menuClassName="right-0 w-72 max-w-[calc(100vw-2rem)]"
            selectedValue={
              defaultDictationPromptId || USE_POST_PROCESSING_PROMPT_VALUE
            }
            onSelect={(value) =>
              updateSetting(
                "transform_default_dictation_prompt_id",
                value === USE_POST_PROCESSING_PROMPT_VALUE ? null : value,
              )
            }
            disabled={isUpdating("transform_default_dictation_prompt_id")}
          />
        </SettingContainer>
      )}
    </SettingsGroup>
  );
};

const TransformAppRulesSettings: React.FC = () => {
  const { t } = useTranslation();
  const osType = useOsType();
  const { getSetting, refreshSettings } = useSettings();

  const rules = getSetting("transform_app_rules") || [];
  const prompts = getSetting("transform_prompts") || [];

  const [isAdding, setIsAdding] = useState(false);
  const [runningApps, setRunningApps] = useState<AppInfo[]>([]);
  const [isLoadingApps, setIsLoadingApps] = useState(false);
  const [selectedRunningApp, setSelectedRunningApp] = useState<string | null>(
    null,
  );
  const [newAppName, setNewAppName] = useState("");
  const [newAppIdentifier, setNewAppIdentifier] = useState("");
  const [newPromptId, setNewPromptId] = useState("");

  const loadRunningApps = useCallback(async () => {
    if (osType !== "macos") return;
    setIsLoadingApps(true);
    try {
      const apps = await commands.getRunningApplications();
      setRunningApps(apps);
    } catch (error) {
      console.error("Failed to list running applications:", error);
    } finally {
      setIsLoadingApps(false);
    }
  }, [osType]);

  const handleOpenAdd = () => {
    setIsAdding(true);
    setSelectedRunningApp(null);
    setNewAppName("");
    setNewAppIdentifier("");
    setNewPromptId(prompts[0]?.id || "");
    void loadRunningApps();
  };

  const handleCancelAdd = () => {
    setIsAdding(false);
    setSelectedRunningApp(null);
    setNewAppName("");
    setNewAppIdentifier("");
  };

  const handlePickRunningApp = (key: string) => {
    setSelectedRunningApp(key);
    const app = runningApps.find(
      (item) => (item.bundle_id || item.name) === key,
    );
    if (app) {
      setNewAppName(app.name);
      setNewAppIdentifier(app.bundle_id || app.name);
    }
  };

  const handleSaveRule = async () => {
    if (!newAppName.trim() || !newAppIdentifier.trim() || !newPromptId) return;

    try {
      const result = await commands.addTransformAppRule(
        newAppName.trim(),
        newAppIdentifier.trim(),
        newPromptId,
      );
      if (result.status === "ok") {
        await refreshSettings();
        handleCancelAdd();
      }
    } catch (error) {
      console.error("Failed to add app rule:", error);
    }
  };

  const handleToggleRule = async (rule: TransformAppRule, enabled: boolean) => {
    try {
      await commands.updateTransformAppRule(
        rule.id,
        rule.app_name,
        rule.app_identifier,
        rule.prompt_id,
        enabled,
      );
      await refreshSettings();
    } catch (error) {
      console.error("Failed to toggle app rule:", error);
    }
  };

  const handleChangeRulePrompt = async (
    rule: TransformAppRule,
    promptId: string,
  ) => {
    try {
      await commands.updateTransformAppRule(
        rule.id,
        rule.app_name,
        rule.app_identifier,
        promptId,
        rule.enabled ?? true,
      );
      await refreshSettings();
    } catch (error) {
      console.error("Failed to update app rule prompt:", error);
    }
  };

  const handleDeleteRule = async (id: string) => {
    try {
      await commands.deleteTransformAppRule(id);
      await refreshSettings();
    } catch (error) {
      console.error("Failed to delete app rule:", error);
    }
  };

  const promptOptions = prompts.map((p) => ({
    value: p.id,
    label: p.name,
  }));

  const runningAppOptions = runningApps.map((app) => ({
    value: app.bundle_id || app.name,
    label: formatAppOptionLabel(app),
  }));

  return (
    <SettingsGroup title={t("settings.transform.appRules.title")}>
      <SettingContainer
        title={t("settings.transform.appRules.title")}
        description={t("settings.transform.appRules.description")}
        descriptionMode="tooltip"
        layout="stacked"
        grouped={true}
      >
        <div className="space-y-3">
          {rules.length === 0 && !isAdding && (
            <div className="p-3 bg-wash rounded-md border border-line">
              <p className="text-sm text-ink-3">
                {t("settings.transform.appRules.empty")}
              </p>
            </div>
          )}

          {rules.length > 0 && (
            <div className="divide-y divide-line border border-line rounded-md">
              {rules.map((rule) => (
                <div
                  key={rule.id}
                  className="flex items-center justify-between gap-3 px-3 py-2.5"
                >
                  <div className="flex items-center gap-3 min-w-0 flex-1">
                    <Switch
                      checked={rule.enabled ?? true}
                      onChange={(checked) => handleToggleRule(rule, checked)}
                      ariaLabel={t("settings.transform.appRules.toggleRule", {
                        app: rule.app_name,
                      })}
                    />
                    <div className="min-w-0">
                      <p className="text-sm font-medium truncate">
                        {rule.app_name}
                      </p>
                      <p className="text-xs text-ink-3 truncate">
                        {rule.app_identifier}
                      </p>
                    </div>
                  </div>

                  <div className="flex items-center gap-2 shrink-0">
                    <Dropdown
                      options={promptOptions}
                      selectedValue={rule.prompt_id}
                      onSelect={(value) => handleChangeRulePrompt(rule, value)}
                      className="w-56"
                    />
                    <Button
                      variant="danger-ghost"
                      size="sm"
                      onClick={() => handleDeleteRule(rule.id)}
                      className="shrink-0"
                      aria-label={t("settings.transform.appRules.deleteRule", {
                        app: rule.app_name,
                      })}
                    >
                      <Trash2 className="w-4 h-4" />
                    </Button>
                  </div>
                </div>
              ))}
            </div>
          )}

          {isAdding ? (
            <div className="space-y-3 p-3 bg-wash rounded-md border border-line">
              {osType === "macos" && (
                <div className="space-y-1.5 flex flex-col">
                  <label className="text-sm font-semibold">
                    {t("settings.transform.appRules.pickRunningApp")}
                  </label>
                  <div className="flex items-center gap-2">
                    <Dropdown
                      options={runningAppOptions}
                      selectedValue={selectedRunningApp}
                      onSelect={handlePickRunningApp}
                      placeholder={t(
                        "settings.transform.appRules.pickRunningAppPlaceholder",
                      )}
                      disabled={isLoadingApps}
                      className="flex-1 min-w-0"
                    />
                    <ResetButton
                      onClick={loadRunningApps}
                      disabled={isLoadingApps}
                      ariaLabel={t("settings.transform.appRules.refreshApps")}
                      className="flex h-9 w-9 items-center justify-center"
                    >
                      <RefreshCcw
                        className={`h-4 w-4 ${isLoadingApps ? "animate-spin" : ""}`}
                      />
                    </ResetButton>
                  </div>
                </div>
              )}

              <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
                <div className="space-y-1.5 flex flex-col">
                  <label className="text-sm font-semibold">
                    {t("settings.transform.appRules.appName")}
                  </label>
                  <Input
                    type="text"
                    value={newAppName}
                    onChange={(e) => setNewAppName(e.target.value)}
                    placeholder={t(
                      "settings.transform.appRules.appNamePlaceholder",
                    )}
                    variant="compact"
                  />
                </div>
                <div className="space-y-1.5 flex flex-col">
                  <label className="text-sm font-semibold">
                    {t("settings.transform.appRules.appIdentifier")}
                  </label>
                  <Input
                    type="text"
                    value={newAppIdentifier}
                    onChange={(e) => setNewAppIdentifier(e.target.value)}
                    placeholder={t(
                      "settings.transform.appRules.appIdentifierPlaceholder",
                    )}
                    variant="compact"
                  />
                </div>
              </div>

              <div className="space-y-1.5 flex flex-col">
                <label className="text-sm font-semibold">
                  {t("settings.transform.appRules.stylePrompt")}
                </label>
                <Dropdown
                  options={promptOptions}
                  selectedValue={newPromptId || null}
                  onSelect={(value) => setNewPromptId(value)}
                  placeholder={t("settings.transform.prompts.selectPrompt")}
                />
              </div>

              <div className="flex gap-2 pt-1">
                <Button
                  variant="primary"
                  size="md"
                  onClick={handleSaveRule}
                  disabled={
                    !newAppName.trim() ||
                    !newAppIdentifier.trim() ||
                    !newPromptId
                  }
                >
                  {t("settings.transform.appRules.saveRule")}
                </Button>
                <Button variant="secondary" size="md" onClick={handleCancelAdd}>
                  {t("settings.transform.appRules.cancel")}
                </Button>
              </div>
            </div>
          ) : (
            <div>
              <Button variant="primary" size="md" onClick={handleOpenAdd}>
                {t("settings.transform.appRules.addRule")}
              </Button>
            </div>
          )}
        </div>
      </SettingContainer>
    </SettingsGroup>
  );
};

export const TransformSettings: React.FC = () => {
  const { t } = useTranslation();

  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      <TransformCoreSettings />
      <TransformPromptsSettings />
      <TransformAppRulesSettings />

      <SettingsGroup title={t("settings.transform.provider.title")}>
        <Alert variant="info" contained>
          {t("settings.transform.provider.sharedNote")}
        </Alert>
        <PostProcessingSettingsApi />
      </SettingsGroup>
    </div>
  );
};
