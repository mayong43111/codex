#!/usr/bin/env bash
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
export CODEX_AGENT_BACKEND=maf
export CODEX_MAF_BRIDGE_DLL="${CODEX_MAF_BRIDGE_DLL:-$root/codex-dotnet/SkillsBridge/bin/Debug/net10.0/Codex.SkillsBridge.dll}"
export CODEX_MAF_AZ="${CODEX_MAF_AZ:-$HOME/.local/share/codex-tools/azure-cli/bin/az}"
: "${CODEX_MAF_FOUNDRY_ENDPOINT:?Set the HTTPS Azure OpenAI Responses endpoint}"
: "${CODEX_MAF_DEPLOYMENT:?Set the deployed model name}"

if [[ ! "$CODEX_MAF_FOUNDRY_ENDPOINT" =~ ^https://[A-Za-z0-9-]+\.openai\.azure\.com/openai/v1/responses$ ]] ||
   [[ ! "$CODEX_MAF_DEPLOYMENT" =~ ^[A-Za-z0-9._-]+$ ]]; then
    printf 'Invalid Foundry endpoint or deployment name\n' >&2
    exit 1
fi
codex="${CODEX_MAF_CODEX_BIN:-$root/codex-rs/target/debug/codex}"
if [[ ! -x "$codex" || ! -f "$CODEX_MAF_BRIDGE_DLL" || ! -x "$CODEX_MAF_AZ" ]]; then
    printf 'Build codex-cli and Codex.SkillsBridge, and configure the Azure CLI path first.\n' >&2
    exit 1
fi

exec "$codex" --no-daemon --no-alt-screen \
    -m "$CODEX_MAF_DEPLOYMENT" \
    -c 'model_reasoning_effort="none"' \
    -c 'model_provider="maf_foundry"' \
    -c 'model_providers.maf_foundry.name="Azure MAF (experimental)"' \
    -c 'model_providers.maf_foundry.wire_api="responses"' \
    -c "model_providers.maf_foundry.base_url=\"${CODEX_MAF_FOUNDRY_ENDPOINT%/responses}\"" \
    -c 'model_providers.maf_foundry.requires_openai_auth=false' \
    "$@"