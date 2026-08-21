/**
 * useConfigStore.cascade — the shared default-selection cascade
 * (spec 06 GAP-F2 / 01 GAP-01).
 *
 * Under the omp model-roles capability the directory's `roles.default`
 * assignment is the ONLY default-model input; every legacy layer
 * (project/settings default, agent pin, opencode config pointer,
 * first-provider fallback) is retired while the capability is on. With no
 * configured default role the resolver returns undefined identifiers — the
 * legal follow-the-engine state. Legacy behavior applies whenever the
 * capability is off or unresolved.
 */

import type { Agent, Provider } from '@/lib/opencode/wire';
import { isPrimaryMode } from '@/components/chat/mobileControlsUtils';
import { isOmpModelRolesEnabled } from '@/lib/omp/capabilityGate';
import { parseModelIdentifier } from '@/lib/modelIdentifier';

export const FALLBACK_PROVIDER_ID = 'opencode';
export const FALLBACK_MODEL_ID = 'big-pickle';

export type ProviderModel = Provider['models'][string];
export type ProviderWithModelList = Omit<Provider, 'models'> & { models: ProviderModel[] };
export const parseModelString = (modelString: string): { providerId: string; modelId: string } | null => {
    return parseModelIdentifier(modelString);
};

export const hasProviderModel = (
    providers: ProviderWithModelList[],
    providerId: string,
    modelId: string
): boolean => {
    const provider = providers.find((item) => item.id === providerId);
    if (!provider) {
        return false;
    }
    return provider.models.some((model) => model.id === modelId);
};

export type DefaultAgentModelSelection = {
    agentName: string | undefined;
    providerId?: string;
    modelId?: string;
    variant?: string;
};

// Shared default-selection cascade used both at startup (loadAgents) and when opening a
// fresh draft (applyDefaultModelAgentSelection), so the two paths stay identical.
//
//   Agent: settings.defaultAgent → opencode default_agent → build → first primary → first
//   Model: project.defaultModel → settings.defaultModel → resolved agent's pinned model+variant → opencode config.model
//          → opencode/big-pickle → first
//
// The opencode default_agent / default model (config fields on the OpenCode server) are honored
// only when our own settings have no valid default. OpenCode itself resolves a model the same way:
// an agent's pinned model wins, otherwise the global `model` config applies — so we check the
// agent's model before opencodeDefaultModel. When the agent supplies the model, its `variant` is
// carried through too (if the model actually exposes that variant).
export const resolveDefaultAgentModelSelection = ({
    agents,
    providers,
    projectDefaultModel,
    settingsDefaultAgent,
    settingsDefaultModel,
    settingsDefaultVariant,
    opencodeDefaultAgent,
    opencodeDefaultModel,
    ompDefaultModel,
}: {
    agents: Agent[];
    providers: ProviderWithModelList[];
    projectDefaultModel?: string;
    settingsDefaultAgent?: string;
    settingsDefaultModel?: string;
    settingsDefaultVariant?: string;
    opencodeDefaultAgent?: string;
    opencodeDefaultModel?: string;
    /** omp model-roles default for the directory (roles.default resolution). */
    ompDefaultModel?: { providerId: string; modelId: string };
}): DefaultAgentModelSelection => {
    if (agents.length === 0) {
        return { agentName: undefined };
    }

    const resolveVariant = (providerId: string, modelId: string, variant?: string): string | undefined => {
        if (!variant) {
            return undefined;
        }
        const model = providers
            .find((provider) => provider.id === providerId)
            ?.models.find((entry) => entry.id === modelId) as { variants?: Record<string, unknown> } | undefined;
        return model?.variants && Object.prototype.hasOwnProperty.call(model.variants, variant)
            ? variant
            : undefined;
    };

    // --- Agent cascade ---
    // Under the omp model-roles capability there is no default agent: the
    // build/plan dichotomy is retired (master D3 row 1) and sessions run
    // standard, so the cascade must not synthesize one. Probe unresolved →
    // legacy behavior.
    const ompModelRoles = isOmpModelRolesEnabled();

    let resolvedAgent: Agent | undefined;
    if (!ompModelRoles) {
        const primaryAgents = agents.filter((agent) => isPrimaryMode(agent.mode));
        if (settingsDefaultAgent) {
            resolvedAgent = agents.find((agent) => agent.name === settingsDefaultAgent);
        }
        if (!resolvedAgent && opencodeDefaultAgent) {
            const candidate = agents.find((agent) => agent.name === opencodeDefaultAgent);
            // OpenCode requires the default agent to be a visible primary agent.
            if (candidate && isPrimaryMode(candidate.mode) && candidate.hidden !== true) {
                resolvedAgent = candidate;
            }
        }
        if (!resolvedAgent) {
            resolvedAgent = primaryAgents.find((agent) => agent.name === 'build') || primaryAgents[0] || agents[0];
        }
    }

    // --- Model cascade ---
    let providerId: string | undefined;
    let modelId: string | undefined;
    let variant: string | undefined;

    if (ompModelRoles) {
        // GAP-F2/01 GAP-01: the omp model-roles face owns default-model
        // resolution. Only the directory's roles.default assignment may pin a
        // model; every legacy layer (project/settings default, agent pin,
        // opencode config pointer, first-provider fallback) is retired while
        // the capability is on. No configured default role = the legal
        // follow-the-engine state (identifiers stay undefined).
        if (ompDefaultModel && hasProviderModel(providers, ompDefaultModel.providerId, ompDefaultModel.modelId)) {
            providerId = ompDefaultModel.providerId;
            modelId = ompDefaultModel.modelId;
        }
        return { agentName: resolvedAgent?.name, providerId, modelId, variant };
    }

    const effectiveDefaultModel = projectDefaultModel || settingsDefaultModel;

    if (effectiveDefaultModel) {
        const parsed = parseModelString(effectiveDefaultModel);
        if (parsed && hasProviderModel(providers, parsed.providerId, parsed.modelId)) {
            providerId = parsed.providerId;
            modelId = parsed.modelId;
            variant = resolveVariant(providerId, modelId, projectDefaultModel ? undefined : settingsDefaultVariant);
        }
    }
    if (!providerId
        && resolvedAgent?.model?.providerID
        && resolvedAgent.model?.modelID
        && hasProviderModel(providers, resolvedAgent.model.providerID, resolvedAgent.model.modelID)) {
        providerId = resolvedAgent.model.providerID;
        modelId = resolvedAgent.model.modelID;
        variant = resolveVariant(providerId, modelId, resolvedAgent.variant);
    }

    // OpenCode's global default model — used when neither our settings nor the agent pin a model.
    if (!providerId && opencodeDefaultModel) {
        const parsed = parseModelString(opencodeDefaultModel);
        if (parsed && hasProviderModel(providers, parsed.providerId, parsed.modelId)) {
            providerId = parsed.providerId;
            modelId = parsed.modelId;
        }
    }

    if (!providerId) {
        if (hasProviderModel(providers, FALLBACK_PROVIDER_ID, FALLBACK_MODEL_ID)) {
            providerId = FALLBACK_PROVIDER_ID;
            modelId = FALLBACK_MODEL_ID;
        } else {
            const firstProvider = providers[0];
            const firstModel = firstProvider?.models[0];
            if (firstProvider && firstModel) {
                providerId = firstProvider.id;
                modelId = firstModel.id;
            }
        }
    }

    return { agentName: resolvedAgent?.name, providerId, modelId, variant };
};
