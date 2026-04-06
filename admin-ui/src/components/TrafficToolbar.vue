<script setup lang="ts">
import { computed } from "vue";
import { ElSegmented } from "element-plus/es/components/segmented/index";
import { ElOption, ElSelect } from "element-plus/es/components/select/index";

import {
  DEFAULT_TRAFFIC_GRANULARITY_BY_RANGE,
  TRAFFIC_GRANULARITY_LABELS,
  TRAFFIC_LAYER_OPTIONS,
  TRAFFIC_GRANULARITY_OPTIONS,
  TRAFFIC_RANGE_OPTIONS,
  type TrafficFilters,
  type TrafficGranularity,
  type TrafficLayer,
  type TrafficRangePreset,
} from "@/types";

const props = defineProps<{
  modelValue: TrafficFilters;
  selectedLayer: TrafficLayer;
}>();

const emit = defineEmits<{
  "update:modelValue": [value: TrafficFilters];
  "update:selectedLayer": [value: TrafficLayer];
}>();

const granularityOptions = computed(() => TRAFFIC_GRANULARITY_OPTIONS[props.modelValue.range]);

function updateRange(current: TrafficFilters, nextRange: TrafficRangePreset) {
  const nextOptions = TRAFFIC_GRANULARITY_OPTIONS[nextRange];
  emit("update:modelValue", {
    range: nextRange,
    granularity: nextOptions.includes(current.granularity)
      ? current.granularity
      : DEFAULT_TRAFFIC_GRANULARITY_BY_RANGE[nextRange],
  });
}

function updateGranularity(current: TrafficFilters, nextGranularity: TrafficGranularity) {
  emit("update:modelValue", {
    ...current,
    granularity: nextGranularity,
  });
}
</script>

<template>
  <div class="metrics-toolbar">
    <el-segmented
      :model-value="selectedLayer"
      :options="TRAFFIC_LAYER_OPTIONS"
      @update:model-value="(value: TrafficLayer) => emit('update:selectedLayer', value)"
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
  </div>
</template>
