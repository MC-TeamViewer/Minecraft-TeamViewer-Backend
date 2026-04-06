<script setup lang="ts">
import ElDatePicker from "element-plus/es/components/date-picker/index";
import { ElOption, ElSelect } from "element-plus/es/components/select/index";
import { DAILY_RANGE_OPTIONS, HOURLY_RANGE_OPTIONS, type MetricsFilters } from "@/types";
import { alignHourlyDateTime, normalizeDateInput } from "@/time";

defineProps<{
  modelValue: MetricsFilters;
  roomOptions: string[];
}>();

const emit = defineEmits<{
  "update:modelValue": [value: MetricsFilters];
}>();

function updatePatch(current: MetricsFilters, patch: Partial<MetricsFilters>) {
  emit("update:modelValue", {
    ...current,
    ...patch,
  });
}
</script>

<template>
  <div class="metrics-toolbar">
    <el-select
      :model-value="modelValue.roomCode"
      class="filter-control"
      clearable
      placeholder="全局房间"
      @update:model-value="(value: string | undefined) => updatePatch(modelValue, { roomCode: value || '' })"
    >
      <el-option label="全局房间" value="" />
      <el-option v-for="roomCode in roomOptions" :key="roomCode" :label="roomCode" :value="roomCode" />
    </el-select>

    <el-select
      :model-value="modelValue.dailyDays"
      class="filter-control short-filter"
      @update:model-value="(value: number | string) => updatePatch(modelValue, { dailyDays: Number(value) })"
    >
      <el-option v-for="value in DAILY_RANGE_OPTIONS" :key="value" :label="`${value} 天 DAU`" :value="value" />
    </el-select>

    <el-date-picker
      :model-value="modelValue.dailyStartDate || undefined"
      class="filter-control"
      clearable
      type="date"
      placeholder="DAU 开始日期"
      value-format="YYYY-MM-DD"
      @update:model-value="
        (value: string | undefined) => updatePatch(modelValue, { dailyStartDate: normalizeDateInput(value) })
      "
    />

    <el-select
      :model-value="modelValue.hourlyHours"
      class="filter-control short-filter"
      @update:model-value="(value: number | string) => updatePatch(modelValue, { hourlyHours: Number(value) })"
    >
      <el-option v-for="value in HOURLY_RANGE_OPTIONS" :key="value" :label="`${value} 小时活跃`" :value="value" />
    </el-select>

    <el-date-picker
      :model-value="modelValue.hourlyStartAt || undefined"
      class="filter-control"
      clearable
      type="datetime"
      placeholder="小时活跃开始时间"
      format="YYYY-MM-DD HH:mm"
      value-format="YYYY-MM-DDTHH:mm:ss"
      @update:model-value="
        (value: string | undefined) => updatePatch(modelValue, { hourlyStartAt: alignHourlyDateTime(value) })
      "
    />
  </div>
</template>
