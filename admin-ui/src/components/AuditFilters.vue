<script setup lang="ts">
import ElButton from "element-plus/es/components/button/index";
import ElSegmented from "element-plus/es/components/segmented/index";
import { ElCheckboxButton, ElCheckboxGroup } from "element-plus/es/components/checkbox/index";
import { ElOption, ElSelect } from "element-plus/es/components/select/index";
import { computed } from "vue";
import type { CheckboxValueType } from "element-plus";

import type { AuditFilters } from "@/types";

const props = defineProps<{
  modelValue: AuditFilters;
  eventTypes: string[];
}>();

const emit = defineEmits<{
  "update:modelValue": [value: AuditFilters];
  refresh: [];
}>();

const model = computed({
  get: () => props.modelValue,
  set: (value: AuditFilters) => emit("update:modelValue", value),
});

function updatePatch(patch: Partial<AuditFilters>) {
  const nextActorTypes = patch.actorTypes ?? model.value.actorTypes;
  model.value = {
    ...model.value,
    ...patch,
    actorTypes: nextActorTypes.length > 0 ? nextActorTypes : model.value.actorTypes,
  };
}
</script>

<template>
  <div class="audit-filter-row">
    <el-select
      :model-value="model.eventType"
      placeholder="全部事件"
      clearable
      filterable
      class="filter-control"
      @update:model-value="(value: string | undefined) => updatePatch({ eventType: value || '' })"
    >
      <el-option label="全部事件" value="" />
      <el-option v-for="eventType in eventTypes" :key="eventType" :label="eventType" :value="eventType" />
    </el-select>

    <el-checkbox-group
      :model-value="model.actorTypes"
      class="actor-type-group"
      @update:model-value="(value: CheckboxValueType[]) => updatePatch({ actorTypes: value as string[] })"
    >
      <el-checkbox-button label="player">游戏端</el-checkbox-button>
      <el-checkbox-button label="web_map">网页端</el-checkbox-button>
      <el-checkbox-button label="system">系统</el-checkbox-button>
      <el-checkbox-button label="admin">管理端</el-checkbox-button>
    </el-checkbox-group>

    <el-segmented
      :model-value="model.success"
      :options="[
        { label: '全部结果', value: '' },
        { label: '成功', value: 'true' },
        { label: '失败', value: 'false' },
      ]"
      @update:model-value="(value: string | number | boolean) => updatePatch({ success: String(value) as AuditFilters['success'] })"
    />

    <el-button type="primary" plain @click="emit('refresh')">刷新日志</el-button>
  </div>
</template>
