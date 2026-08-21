<script setup lang="ts">
import ElCard from "element-plus/es/components/card/index";
import ElTag from "element-plus/es/components/tag/index";
import { ElTable, ElTableColumn } from "element-plus/es/components/table/index";
import { computed } from "vue";

import type { ConnectionDetail, OverviewPayload, ProtobufConnectionTraffic, ProtobufTrafficPayload } from "@/types";

const props = defineProps<{
  overview: OverviewPayload | null;
  protobufTraffic: ProtobufTrafficPayload | null;
}>();

const rows = computed(() =>
  (props.overview?.connectionDetails ?? []).filter((row) => row.channel === "external_source"),
);

const healthLabels: Record<string, string> = {
  EXTERNAL_SOURCE_HEALTH_STARTING: "启动中",
  EXTERNAL_SOURCE_HEALTH_HEALTHY: "正常",
  EXTERNAL_SOURCE_HEALTH_DEGRADED: "降级",
  EXTERNAL_SOURCE_HEALTH_UNAVAILABLE: "不可用",
};

function healthLabel(row: ConnectionDetail): string {
  return healthLabels[row.health ?? ""] ?? "未知";
}

function healthType(row: ConnectionDetail): "success" | "warning" | "danger" | "info" {
  if (row.health === "EXTERNAL_SOURCE_HEALTH_HEALTHY") return "success";
  if (row.health === "EXTERNAL_SOURCE_HEALTH_DEGRADED") return "warning";
  if (row.health === "EXTERNAL_SOURCE_HEALTH_UNAVAILABLE") return "danger";
  return "info";
}

function formatTimestamp(value: number | null | undefined): string {
  if (!value) return "-";
  return new Date(value * 1000).toLocaleString();
}

const trafficByActor = computed(() => new Map(
  (props.protobufTraffic?.connections ?? []).map((item) => [`${item.channel}:${item.actorId}`, item]),
));

function trafficFor(row: ConnectionDetail): ProtobufConnectionTraffic | undefined {
  return trafficByActor.value.get(`${row.channel}:${row.actorId}`);
}

function formatBytes(value: number | null | undefined): string {
  const amount = Number(value ?? 0);
  if (!Number.isFinite(amount) || amount <= 0) return "0 B";
  const units = ["B", "KiB", "MiB", "GiB"];
  const index = Math.min(Math.floor(Math.log(amount) / Math.log(1024)), units.length - 1);
  const scaled = amount / (1024 ** index);
  return `${scaled >= 10 || index === 0 ? scaled.toFixed(0) : scaled.toFixed(1)} ${units[index]}`;
}

function protobufSummary(row: ConnectionDetail): string {
  const metric = trafficFor(row)?.total;
  if (!metric) return "-";
  const rate = metric.messagesPerSecond > 0 ? `${metric.messagesPerSecond.toFixed(2)} 包/s` : "0 包/s";
  return `${rate} · ${metric.messageCount} 包`;
}
</script>

<template>
  <el-card shadow="never" class="surface-card">
    <template #header>
      <div class="section-header">
        <h2>外部数据源</h2>
      </div>
    </template>

    <el-table :data="rows" border table-layout="fixed" class="admin-table" empty-text="暂无外部数据源">
      <el-table-column prop="displayName" label="名称" min-width="180" show-overflow-tooltip />
      <el-table-column prop="roomCode" label="房间" min-width="120" show-overflow-tooltip />
      <el-table-column label="连接" width="90">
        <template #default="{ row }">
          <el-tag :type="row.connected === false ? 'info' : 'success'" effect="plain">
            {{ row.connected === false ? "已断开" : "已连接" }}
          </el-tag>
        </template>
      </el-table-column>
      <el-table-column label="健康状态" width="110">
        <template #default="{ row }">
          <el-tag :type="healthType(row)" effect="plain">{{ healthLabel(row) }}</el-tag>
        </template>
      </el-table-column>
      <el-table-column label="最近健康" min-width="170">
        <template #default="{ row }">{{ formatTimestamp(row.lastHealthyAt) }}</template>
      </el-table-column>
      <el-table-column prop="failureCode" label="失败代码" min-width="140" show-overflow-tooltip>
        <template #default="{ row }">{{ row.failureCode || "-" }}</template>
      </el-table-column>
      <el-table-column prop="protocolVersion" label="协议" width="90" />
      <el-table-column label="Protobuf 发包" min-width="150">
        <template #default="{ row }">{{ protobufSummary(row) }}</template>
      </el-table-column>
      <el-table-column label="累计字节" width="120">
        <template #default="{ row }">{{ formatBytes(trafficFor(row)?.total.byteCount) }}</template>
      </el-table-column>
      <el-table-column prop="programVersion" label="程序版本" min-width="180" show-overflow-tooltip />
      <el-table-column prop="actorId" label="Source ID" min-width="240" show-overflow-tooltip>
        <template #default="{ row }"><span class="mono-text">{{ row.actorId }}</span></template>
      </el-table-column>
    </el-table>
  </el-card>
</template>
