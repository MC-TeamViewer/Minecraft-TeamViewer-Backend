import { ref } from "vue";

import type { AuditPayload } from "@/types";


export function useAuditState() {
  const auditPayload = ref<AuditPayload | null>(null);
  const eventTypes = ref<string[]>([]);

  function applyAudit(payload: AuditPayload) {
    auditPayload.value = payload;
    eventTypes.value = payload.availableEventTypes ?? [];
  }

  return {
    auditPayload,
    eventTypes,
    applyAudit,
  };
}
