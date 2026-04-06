import { ref } from "vue";

import type { AuditPayload } from "@/types";


export function useAuditState() {
  const auditPayload = ref<AuditPayload | null>(null);
  const eventTypes = ref<string[]>([]);

  function applyAudit(payload: AuditPayload) {
    auditPayload.value = payload;
    eventTypes.value = payload.availableEventTypes ?? [];
  }

  function resetAudit() {
    auditPayload.value = null;
    eventTypes.value = [];
  }

  return {
    auditPayload,
    eventTypes,
    applyAudit,
    resetAudit,
  };
}
