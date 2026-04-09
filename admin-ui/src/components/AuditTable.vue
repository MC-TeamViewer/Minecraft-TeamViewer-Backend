<script setup lang="ts">
import ElButton from "element-plus/es/components/button/index";
import ElCard from "element-plus/es/components/card/index";
import ElPagination from "element-plus/es/components/pagination/index";
import ElTag from "element-plus/es/components/tag/index";
import { ElTable, ElTableColumn } from "element-plus/es/components/table/index";
import { computed, ref, watch } from "vue";

import type { AuditItem, AuditPayload } from "@/types";

const props = defineProps<{
  audit: AuditPayload | null;
}>();

const currentPage = ref(1);
const mappingCurrentPage = ref(1);
const pageSize = 25;
const mappingPageSize = 10;
const prettyExpandedRows = ref<Record<number, boolean>>({});
const actorDisplayMode = ref<"id" | "username">("id");

function formatOccurredAt(value: number): string {
  return new Date(value).toLocaleString("zh-CN", {
    hour12: false,
  });
}

function formatTimestamp(value: number): string {
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

const pagedMappings = computed(() => {
  const items = props.audit?.playerIdentityMappings ?? [];
  const start = (mappingCurrentPage.value - 1) * mappingPageSize;
  return items.slice(start, start + mappingPageSize);
});

watch(
  () => props.audit?.items,
  () => {
    currentPage.value = 1;
  },
);

watch(
  () => props.audit?.playerIdentityMappings,
  () => {
    mappingCurrentPage.value = 1;
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

function handleMappingPageChange(page: number) {
  mappingCurrentPage.value = page;
}

function resolveActorDisplay(row: AuditItem): string {
  if (row.actorType === "player" && actorDisplayMode.value === "username") {
    const resolved = String(row.resolvedActorName || "").trim();
    if (resolved) {
      return resolved;
    }
  }
  return String(row.actorId || "");
}
</script>

<template>
  <el-card shadow="never" class="surface-card">
    <template #header>
      <div class="section-header">
        <div>
          <h2>玩家身份映射</h2>
          <p>当前已学到的 submitPlayerId 与 username 映射。</p>
        </div>
      </div>
    </template>

    <el-table
      :data="pagedMappings"
      border
      row-key="playerId"
      table-layout="fixed"
      class="admin-table"
      empty-text="暂无身份映射"
    >
      <el-table-column prop="playerId" label="UUID" min-width="280" show-overflow-tooltip resizable />
      <el-table-column prop="username" label="Username" min-width="180" show-overflow-tooltip resizable />
      <el-table-column label="最后更新时间" min-width="190" resizable>
        <template #default="{ row }">
          {{ formatTimestamp(row.updatedAt) }}
        </template>
      </el-table-column>
    </el-table>

    <div class="audit-pagination">
      <el-pagination
        layout="prev, pager, next"
        :total="audit?.playerIdentityMappings?.length ?? 0"
        :page-size="mappingPageSize"
        :current-page="mappingCurrentPage"
        size="small"
        background
        @update:current-page="handleMappingPageChange"
      />
    </div>
  </el-card>

  <el-card shadow="never" class="surface-card">
    <template #header>
      <div class="section-header">
        <div>
          <h2>审计日志</h2>
          <p>最新 100 条匹配当前筛选条件的审计事件。</p>
        </div>
        <div class="audit-detail-toolbar">
          <el-button
            :type="actorDisplayMode === 'id' ? 'primary' : 'default'"
            plain
            @click="actorDisplayMode = 'id'"
          >
            显示原始 Actor ID
          </el-button>
          <el-button
            :type="actorDisplayMode === 'username' ? 'primary' : 'default'"
            plain
            @click="actorDisplayMode = 'username'"
          >
            显示 username
          </el-button>
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
      <el-table-column label="Actor" min-width="180" show-overflow-tooltip resizable>
        <template #default="{ row }">
          {{ resolveActorDisplay(row) }}
        </template>
      </el-table-column>
      <el-table-column prop="roomCode" label="房间" min-width="140" show-overflow-tooltip resizable />
      <el-table-column prop="remoteAddr" label="地址" min-width="160" show-overflow-tooltip resizable />
    </el-table>

    <div class="audit-pagination">
      <el-pagination
        layout="prev, pager, next"
        :total="audit?.items?.length ?? 0"
        :page-size="pageSize"
        :current-page="currentPage"
        size="small"
        background
        @update:current-page="handlePageChange"
      />
    </div>
  </el-card>
</template>
