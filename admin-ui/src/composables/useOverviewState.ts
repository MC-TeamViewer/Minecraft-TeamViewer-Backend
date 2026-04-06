import { computed, ref } from "vue";

import type { OverviewPayload } from "@/types";


export function useOverviewState() {
  const overview = ref<OverviewPayload | null>(null);

  function applyOverview(payload: OverviewPayload) {
    overview.value = payload;
  }

  const roomOptions = computed(() => {
    const rooms = overview.value?.rooms ?? [];
    return rooms.map((item) => item.roomCode).filter(Boolean);
  });

  return {
    overview,
    roomOptions,
    applyOverview,
  };
}
