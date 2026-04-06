<script setup lang="ts">
import type { AuditPayload } from "@/types";

defineProps<{
  audit: AuditPayload | null;
}>();

function formatOccurredAt(value: number): string {
  return new Date(value).toLocaleString("zh-CN", {
    hour12: false,
  });
}

function formatDetail(detail: Record<string, unknown>): string {
  try {
    return JSON.stringify(detail, null, 2);
  } catch (_error) {
    return "{}";
  }
}
</script>

<template>
  <el-card shadow="never" class="surface-card">
    <template #header>
      <div class="section-header">
        <div>
          <h2>审计日志</h2>
          <p>最新 100 条匹配当前筛选条件的审计事件。</p>
        </div>
      </div>
    </template>

    <el-table
      :data="audit?.items ?? []"
      border
      row-key="id"
      table-layout="fixed"
      class="admin-table audit-table"
      empty-text="暂无审计事件"
    >
      <el-table-column type="expand" width="44">
        <template #default="{ row }">
          <pre class="audit-detail">{{ formatDetail(row.detail || {}) }}</pre>
        </template>
      </el-table-column>
      <el-table-column prop="id" label="ID" width="88" resizable />
      <el-table-column label="时间" min-width="190" resizable>
        <template #default="{ row }">
          {{ formatOccurredAt(row.occurredAt) }}
        </template>
      </el-table-column>
      <el-table-column prop="eventType" label="事件" min-width="220" show-overflow-tooltip resizable />
      <el-table-column prop="actorType" label="角色" width="110" resizable />
      <el-table-column label="结果" width="92" resizable>
        <template #default="{ row }">
          <el-tag :type="row.success ? 'success' : 'danger'" effect="plain" round>
            {{ row.success ? "成功" : "失败" }}
          </el-tag>
        </template>
      </el-table-column>
      <el-table-column prop="actorId" label="Actor ID" min-width="180" show-overflow-tooltip resizable />
      <el-table-column prop="roomCode" label="房间" min-width="140" show-overflow-tooltip resizable />
      <el-table-column prop="remoteAddr" label="地址" min-width="160" show-overflow-tooltip resizable />
    </el-table>
  </el-card>
</template>
