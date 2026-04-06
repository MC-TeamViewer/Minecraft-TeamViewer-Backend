<script setup lang="ts">
import ElButton from "element-plus/es/components/button/index";
import ElCard from "element-plus/es/components/card/index";
import ElPagination from "element-plus/es/components/pagination/index";
import ElTag from "element-plus/es/components/tag/index";
import { ElTable, ElTableColumn } from "element-plus/es/components/table/index";
import { computed, ref, watch } from "vue";

import type { AuditPayload } from "@/types";

const props = defineProps<{
  audit: AuditPayload | null;
}>();

const currentPage = ref(1);
const pageSize = 25;
const prettyExpandedRows = ref<Record<number, boolean>>({});

function formatOccurredAt(value: number): string {
  return new Date(value).toLocaleString("zh-CN", {
    hour12: false,
  });
}

function formatDetail(detail: Record<string, unknown>, pretty = true): string {
  try {
    return pretty ? JSON.stringify(detail, null, 2) : JSON.stringify(detail);
  } catch (_error) {
    return "{}";
  }
}

const pagedItems = computed(() => {
  const items = props.audit?.items ?? [];
  const start = (currentPage.value - 1) * pageSize;
  return items.slice(start, start + pageSize);
});

watch(
  () => props.audit?.items,
  () => {
    currentPage.value = 1;
  },
);

function togglePretty(rowId: number) {
  prettyExpandedRows.value = {
    ...prettyExpandedRows.value,
    [rowId]: !prettyExpandedRows.value[rowId],
  };
}

async function copyDetail(detail: Record<string, unknown>) {
  const text = formatDetail(detail, true);
  await navigator.clipboard.writeText(text);
}

function handlePageChange(page: number) {
  currentPage.value = page;
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
      :data="pagedItems"
      border
      row-key="id"
      table-layout="fixed"
      class="admin-table audit-table"
      empty-text="暂无审计事件"
    >
      <el-table-column type="expand" width="44">
        <template #default="{ row }">
          <div class="audit-detail-toolbar">
            <el-button text type="primary" @click="copyDetail(row.detail || {})">复制 JSON</el-button>
            <el-button text @click="togglePretty(row.id)">
              {{ prettyExpandedRows[row.id] === false ? "格式化查看" : "折叠查看" }}
            </el-button>
          </div>
          <pre class="audit-detail">{{ formatDetail(row.detail || {}, prettyExpandedRows[row.id] !== false) }}</pre>
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

    <div class="audit-pagination">
      <el-pagination
        layout="prev, pager, next"
        :total="audit?.items?.length ?? 0"
        :page-size="pageSize"
        :current-page="currentPage"
        small
        background
        @update:current-page="handlePageChange"
      />
    </div>
  </el-card>
</template>
