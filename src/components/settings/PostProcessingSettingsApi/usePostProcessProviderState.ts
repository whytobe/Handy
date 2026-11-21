import { useCallback, useMemo, useRef, useState } from "react";
import { useSettings } from "../../../hooks/useSettings";
import { useSettingsStore } from "../../../stores/settingsStore";
import {
  commands,
  type PostProcessApiFormat,
  type PostProcessProvider,
} from "@/bindings";
import type { ModelOption } from "./types";
import type { DropdownOption } from "../../ui/Dropdown";

export type ModelTestStatus =
  | { kind: "idle" }
  | { kind: "testing" }
  | { kind: "missing_model" }
  | {
      kind: "success";
      model: string;
      responsePreview: string;
      latencyMs: number;
    }
  | { kind: "error"; message: string };

type PostProcessProviderState = {
  providerOptions: DropdownOption[];
  selectedProviderId: string;
  selectedProvider: PostProcessProvider | undefined;
  isCustomProvider: boolean;
  isAppleProvider: boolean;
  appleIntelligenceUnavailable: boolean;
  baseUrl: string;
  handleBaseUrlChange: (value: string) => void;
  isBaseUrlUpdating: boolean;
  apiFormat: PostProcessApiFormat;
  handleApiFormatChange: (value: PostProcessApiFormat) => void;
  isApiFormatUpdating: boolean;
  apiKey: string;
  handleApiKeyChange: (value: string) => void;
  isApiKeyUpdating: boolean;
  model: string;
  handleModelChange: (value: string) => void;
  modelOptions: ModelOption[];
  isModelUpdating: boolean;
  isFetchingModels: boolean;
  fetchModelsError: string | null;
  testStatus: ModelTestStatus;
  isTestingModel: boolean;
  handleProviderSelect: (providerId: string) => void;
  handleModelSelect: (value: string) => void;
  handleModelCreate: (value: string) => void;
  handleModelDraftChange: (draft: string) => void;
  handleRefreshModels: () => void;
  handleTestModel: () => Promise<void>;
};

const APPLE_PROVIDER_ID = "apple_intelligence";

export const usePostProcessProviderState = (): PostProcessProviderState => {
  const {
    settings,
    isUpdating,
    setPostProcessProvider,
    updatePostProcessBaseUrl,
    updatePostProcessApiFormat,
    updatePostProcessApiKey,
    updatePostProcessModel,
    fetchPostProcessModels,
    postProcessModelOptions,
    postProcessModelsError,
  } = useSettings();

  // Settings are guaranteed to have providers after migration
  const providers = settings?.post_process_providers || [];

  const selectedProviderId = useMemo(() => {
    return settings?.post_process_provider_id || providers[0]?.id || "openai";
  }, [providers, settings?.post_process_provider_id]);

  const selectedProvider = useMemo(() => {
    return (
      providers.find((provider) => provider.id === selectedProviderId) ||
      providers[0]
    );
  }, [providers, selectedProviderId]);

  const isAppleProvider = selectedProvider?.id === APPLE_PROVIDER_ID;
  const [appleIntelligenceUnavailable, setAppleIntelligenceUnavailable] =
    useState(false);
  const [testStatus, setTestStatus] = useState<ModelTestStatus>({
    kind: "idle",
  });
  const modelDraftRef = useRef("");
  const pendingWriteRef = useRef<Promise<unknown>>(Promise.resolve());
  const testRequestIdRef = useRef(0);

  const trackWrite = useCallback(<T>(work: () => Promise<T>): Promise<T> => {
    const next = pendingWriteRef.current.then(work, work);
    pendingWriteRef.current = next;
    return next;
  }, []);

  const resetTestStatus = useCallback(() => {
    testRequestIdRef.current += 1;
    setTestStatus({ kind: "idle" });
  }, []);

  // Use settings directly as single source of truth
  const baseUrl = selectedProvider?.base_url ?? "";
  const apiFormat = selectedProvider?.api_format ?? "openai";
  const apiKey = settings?.post_process_api_keys?.[selectedProviderId] ?? "";
  const model = settings?.post_process_models?.[selectedProviderId] ?? "";

  const providerOptions = useMemo<DropdownOption[]>(() => {
    return providers.map((provider) => ({
      value: provider.id,
      label: provider.label,
    }));
  }, [providers]);

  const handleProviderSelect = useCallback(
    async (providerId: string) => {
      // Clear error state on any selection attempt (allows dismissing the error)
      setAppleIntelligenceUnavailable(false);

      if (providerId === selectedProviderId) return;

      modelDraftRef.current = "";
      resetTestStatus();

      // Check Apple Intelligence availability before selecting
      if (providerId === APPLE_PROVIDER_ID) {
        const available = await commands.checkAppleIntelligenceAvailable();
        if (!available) {
          setAppleIntelligenceUnavailable(true);
          // Don't return - still set the provider so dropdown shows the selection
          // The backend gracefully handles unavailable Apple Intelligence
        }
      }

      await trackWrite(() => setPostProcessProvider(providerId));

      // Auto-fetch available models for the new provider so the model dropdown
      // reflects what's actually valid. Without this, a stale model value from
      // a previous provider/base_url can persist and silently 404 at runtime.
      // Skip when the provider isn't configured yet (no API key / empty base URL)
      // to avoid unnecessary backend errors.
      if (providerId !== APPLE_PROVIDER_ID) {
        const provider = providers.find((p) => p.id === providerId);
        const apiKey = settings?.post_process_api_keys?.[providerId] ?? "";
        const hasBaseUrl = (provider?.base_url ?? "").trim() !== "";
        const hasApiKey = apiKey.trim() !== "";

        if (provider?.id === "custom" ? hasBaseUrl : hasApiKey) {
          void fetchPostProcessModels(providerId);
        }
      }
    },
    [
      selectedProviderId,
      resetTestStatus,
      trackWrite,
      setPostProcessProvider,
      fetchPostProcessModels,
      providers,
      settings,
    ],
  );

  const handleBaseUrlChange = useCallback(
    (value: string) => {
      if (!selectedProvider || selectedProvider.id !== "custom") {
        return;
      }
      const trimmed = value.trim();
      if (trimmed && trimmed !== baseUrl) {
        resetTestStatus();
        void trackWrite(() =>
          updatePostProcessBaseUrl(selectedProvider.id, trimmed),
        );
      }
    },
    [
      selectedProvider,
      baseUrl,
      resetTestStatus,
      trackWrite,
      updatePostProcessBaseUrl,
    ],
  );

  const handleApiFormatChange = useCallback(
    (value: PostProcessApiFormat) => {
      if (value !== apiFormat) {
        resetTestStatus();
        void trackWrite(() =>
          updatePostProcessApiFormat(selectedProviderId, value),
        );
      }
    },
    [
      apiFormat,
      selectedProviderId,
      resetTestStatus,
      trackWrite,
      updatePostProcessApiFormat,
    ],
  );

  const handleApiKeyChange = useCallback(
    (value: string) => {
      const trimmed = value.trim();
      if (trimmed !== apiKey) {
        resetTestStatus();
        void trackWrite(() =>
          updatePostProcessApiKey(selectedProviderId, trimmed),
        );
      }
    },
    [
      apiKey,
      selectedProviderId,
      resetTestStatus,
      trackWrite,
      updatePostProcessApiKey,
    ],
  );

  const handleModelChange = useCallback(
    (value: string) => {
      const trimmed = value.trim();
      if (trimmed !== model) {
        resetTestStatus();
        void trackWrite(() =>
          updatePostProcessModel(selectedProviderId, trimmed),
        );
      }
    },
    [
      model,
      selectedProviderId,
      resetTestStatus,
      trackWrite,
      updatePostProcessModel,
    ],
  );

  const handleModelSelect = useCallback(
    (value: string) => {
      resetTestStatus();
      void trackWrite(() =>
        updatePostProcessModel(selectedProviderId, value.trim()),
      );
    },
    [selectedProviderId, resetTestStatus, trackWrite, updatePostProcessModel],
  );

  const handleModelCreate = useCallback(
    (value: string) => {
      resetTestStatus();
      void trackWrite(() =>
        updatePostProcessModel(selectedProviderId, value.trim()),
      );
    },
    [selectedProviderId, resetTestStatus, trackWrite, updatePostProcessModel],
  );

  const handleModelDraftChange = useCallback(
    (draft: string) => {
      modelDraftRef.current = draft;
      if (draft !== "") {
        resetTestStatus();
      }
    },
    [resetTestStatus],
  );

  const handleRefreshModels = useCallback(() => {
    if (isAppleProvider) return;
    void (async () => {
      await pendingWriteRef.current;
      await fetchPostProcessModels(selectedProviderId);
    })();
  }, [fetchPostProcessModels, isAppleProvider, selectedProviderId]);

  const handleTestModel = useCallback(async () => {
    const requestId = ++testRequestIdRef.current;
    setTestStatus({ kind: "testing" });

    await pendingWriteRef.current;
    if (testRequestIdRef.current !== requestId) return;

    const latestModel = (
      useSettingsStore.getState().settings?.post_process_models?.[
        selectedProviderId
      ] ?? model
    ).trim();
    const draft = modelDraftRef.current.trim();
    const effectiveModel = draft || latestModel;

    if (!isAppleProvider && !effectiveModel) {
      setTestStatus({ kind: "missing_model" });
      return;
    }

    if (draft && draft !== latestModel) {
      modelDraftRef.current = "";
      await trackWrite(() => updatePostProcessModel(selectedProviderId, draft));
      if (testRequestIdRef.current !== requestId) return;
    }

    try {
      const result = await commands.testPostProcessModel(
        selectedProviderId,
        effectiveModel || null,
      );
      if (testRequestIdRef.current !== requestId) return;
      if (result.status === "ok") {
        setTestStatus({
          kind: "success",
          model: result.data.model,
          responsePreview: result.data.response_preview,
          latencyMs: result.data.latency_ms,
        });
      } else {
        setTestStatus({ kind: "error", message: result.error });
      }
    } catch (error) {
      if (testRequestIdRef.current !== requestId) return;
      setTestStatus({
        kind: "error",
        message: error instanceof Error ? error.message : String(error),
      });
    }
  }, [
    isAppleProvider,
    model,
    selectedProviderId,
    trackWrite,
    updatePostProcessModel,
  ]);

  const availableModelsRaw = postProcessModelOptions[selectedProviderId] || [];

  const modelOptions = useMemo<ModelOption[]>(() => {
    const seen = new Set<string>();
    const options: ModelOption[] = [];

    const upsert = (value: string | null | undefined) => {
      const trimmed = value?.trim();
      if (!trimmed || seen.has(trimmed)) return;
      seen.add(trimmed);
      options.push({ value: trimmed, label: trimmed });
    };

    // Add available models from API
    for (const candidate of availableModelsRaw) {
      upsert(candidate);
    }

    // Ensure current model is in the list
    upsert(model);

    return options;
  }, [availableModelsRaw, model]);

  const isBaseUrlUpdating = isUpdating(
    `post_process_base_url:${selectedProviderId}`,
  );
  const isApiFormatUpdating = isUpdating(
    `post_process_api_format:${selectedProviderId}`,
  );
  const isApiKeyUpdating = isUpdating(
    `post_process_api_key:${selectedProviderId}`,
  );
  const isModelUpdating = isUpdating(
    `post_process_model:${selectedProviderId}`,
  );
  const isFetchingModels = isUpdating(
    `post_process_models_fetch:${selectedProviderId}`,
  );

  const isCustomProvider = selectedProvider?.id === "custom";
  const fetchModelsError = postProcessModelsError[selectedProviderId] ?? null;
  const isTestingModel = testStatus.kind === "testing";

  // No automatic fetching - user must click refresh button

  return {
    providerOptions,
    selectedProviderId,
    selectedProvider,
    isCustomProvider,
    isAppleProvider,
    appleIntelligenceUnavailable,
    baseUrl,
    handleBaseUrlChange,
    isBaseUrlUpdating,
    apiFormat,
    handleApiFormatChange,
    isApiFormatUpdating,
    apiKey,
    handleApiKeyChange,
    isApiKeyUpdating,
    model,
    handleModelChange,
    modelOptions,
    isModelUpdating,
    isFetchingModels,
    fetchModelsError,
    testStatus,
    isTestingModel,
    handleProviderSelect,
    handleModelSelect,
    handleModelCreate,
    handleModelDraftChange,
    handleRefreshModels,
    handleTestModel,
  };
};
