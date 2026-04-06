<script setup lang="ts">
import { computed } from "vue";
import ElDatePicker from "element-plus/es/components/date-picker/index";
import { ElSegmented } from "element-plus/es/components/segmented/index";
import { ElOption, ElSelect } from "element-plus/es/components/select/index";

import {
  DEFAULT_TRAFFIC_GRANULARITY_BY_RANGE,
  TRAFFIC_GRANULARITY_LABELS,
  TRAFFIC_HISTORY_DISPLAY_OPTIONS,
  TRAFFIC_GRANULARITY_OPTIONS,
  TRAFFIC_RANGE_OPTIONS,
  type TrafficFilters,
  type TrafficGranularity,
  type TrafficHistoryDisplayMode,
  type TrafficRangePreset,
} from "@/types";
import { alignTrafficDateTime } from "@/time";

const props = defineProps<{
  modelValue: TrafficFilters;
  selectedMode: TrafficHistoryDisplayMode;
}>();

const emit = defineEmits<{
  "update:modelValue": [value: TrafficFilters];
  "update:selectedMode": [value: TrafficHistoryDisplayMode];
}>();

const granularityOptions = computed(() => TRAFFIC_GRANULARITY_OPTIONS[props.modelValue.range]);

function updateRange(current: TrafficFilters, nextRange: TrafficRangePreset) {
  const nextOptions = TRAFFIC_GRANULARITY_OPTIONS[nextRange];
  const nextGranularity = nextOptions.includes(current.granularity)
    ? current.granularity
    : DEFAULT_TRAFFIC_GRANULARITY_BY_RANGE[nextRange];
  emit("update:modelValue", {
    range: nextRange,
    granularity: nextGranularity,
    startAt: alignTrafficDateTime(current.startAt, nextGranularity),
  });
}

function updateGranularity(current: TrafficFilters, nextGranularity: TrafficGranularity) {
  emit("update:modelValue", {
    ...current,
    granularity: nextGranularity,
    startAt: alignTrafficDateTime(current.startAt, nextGranularity),
  });
}
</script>

<template>
  <div class="metrics-toolbar">
    <el-segmented
      :model-value="selectedMode"
      :options="TRAFFIC_HISTORY_DISPLAY_OPTIONS"
      @update:model-value="(value: TrafficHistoryDisplayMode) => emit('update:selectedMode', value)"
    />

    <el-select
      :model-value="modelValue.range"
      class="filter-control short-filter"
      @update:model-value="(value: TrafficRangePreset) => updateRange(modelValue, value)"
    >
      <el-option
        v-for="option in TRAFFIC_RANGE_OPTIONS"
        :key="option.value"
        :label="option.label"
        :value="option.value"
      />
    </el-select>

    <el-select
      :model-value="modelValue.granularity"
      class="filter-control short-filter"
      @update:model-value="(value: TrafficGranularity) => updateGranularity(modelValue, value)"
    >
      <el-option
        v-for="value in granularityOptions"
        :key="value"
        :label="TRAFFIC_GRANULARITY_LABELS[value]"
        :value="value"
      />
    </el-select>

    <el-date-picker
      :model-value="modelValue.startAt || undefined"
      class="filter-control"
      clearable
      type="datetime"
      placeholder="历史流量开始时间"
      format="YYYY-MM-DD HH:mm"
      value-format="YYYY-MM-DDTHH:mm:ss"
      @update:model-value="
        (value: string | undefined) => emit('update:modelValue', {
          ...modelValue,
          startAt: alignTrafficDateTime(value, modelValue.granularity),
        })
      "
    />
  </div>
</template>
