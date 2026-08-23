<script setup lang="ts">
import ElCard from "element-plus/es/components/card/index";
import { ElTable, ElTableColumn } from "element-plus/es/components/table/index";
import { computed, ref } from "vue";

import { connectionColumns } from "@/connectionColumns";
import type {
  ConnectionDetail,
  OverviewPayload,
  ProtobufConnectionTraffic,
  ProtobufMetric,
  ProtobufTrafficPayload,
} from "@/types";

const props = defineProps<{
  overview: OverviewPayload | null;
  protobufTraffic: ProtobufTrafficPayload | null;
}>();

const rows = computed(() => (props.overview?.connectionDetails ?? []).filter((row) => row.channel !== "external_source"));
const expandedKeys = ref<string[]>([]);
const trafficByActor = computed(() => new Map(
  (props.protobufTraffic?.connections ?? []).map((item) => [`${item.channel}:${item.actorId}`, item]),
));
const compatibilityRuleSummary = computed(() => new Map(
  (props.overview?.protocolCompatibility?.rules ?? []).map((rule) => [rule.id, rule.summary]),
));

function formatChannel(value: ConnectionDetail["channel"]): string {
  if (value === "player") {
    return "游戏端";
  }
  if (value === "web_map") {
    return "网页端";
  }
  return value || "-";
}

function handleExpandChange(row: ConnectionDetail, expandedRows: ConnectionDetail[]) {
  expandedKeys.value = expandedRows.map((item) => item.actorId);
}

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

function formatMessageRate(value: number | null | undefined): string {
  const amount = Number(value ?? 0);
  if (!Number.isFinite(amount) || amount <= 0) return "0 包/s";
  return `${amount >= 10 ? amount.toFixed(1) : amount.toFixed(2)} 包/s`;
}

function formatByteRate(value: number | null | undefined): string {
  return `${formatBytes(value)}/s`;
}

function formatTimestamp(value: number | null | undefined): string {
  if (!value) return "-";
  return new Date(value * 1000).toLocaleString("zh-CN", { hour12: false });
}

function snapshotSummary(metric: ProtobufMetric | undefined): string {
  if (!metric || metric.messageCount === 0) return "0";
  return `${metric.messageCount} 包 / ${formatBytes(metric.byteCount)}`;
}

function activeCompatibilitySummary(row: ConnectionDetail): string {
  const rules = row.activeCompatibilityRules ?? [];
  if (rules.length === 0) return "无需兼容适配";
  return rules.map((rule) => compatibilityRuleSummary.value.get(rule) ?? rule).join("；");
}
</script>

<template>
  <el-card shadow="never" class="surface-card">
    <template #header>
      <div class="section-header">
        <div>
          <h2>当前连接状态</h2>
          <p>只统计已经完成 WebSocket 握手并登记到服务端内存态的连接；Protobuf 指标为最近 {{ protobufTraffic?.sampleWindowSec ?? 10 }} 秒与本次进程累计。</p>
        </div>
      </div>
    </template>

    <section class="status-summary-grid">
      <div class="status-summary-card">
        <span class="status-summary-label">当前总连接</span>
        <strong class="status-summary-value">
          {{ (overview?.playerConnections ?? 0) + (overview?.webMapConnections ?? 0) }}
        </strong>
      </div>
      <div class="status-summary-card">
        <span class="status-summary-label">游戏端</span>
        <strong class="status-summary-value">{{ overview?.playerConnections ?? 0 }}</strong>
      </div>
      <div class="status-summary-card">
        <span class="status-summary-label">网页端</span>
        <strong class="status-summary-value">{{ overview?.webMapConnections ?? 0 }}</strong>
      </div>
      <div class="status-summary-card">
        <span class="status-summary-label">活跃房间</span>
        <strong class="status-summary-value">{{ overview?.activeRooms ?? 0 }}</strong>
      </div>
      <div class="status-summary-card">
        <span class="status-summary-label">兼容连接</span>
        <strong class="status-summary-value">{{ overview?.protocolCompatibility?.compatibilityConnectionCount ?? 0 }}</strong>
      </div>
      <div class="status-summary-card">
        <span class="status-summary-label">规则命中</span>
        <strong class="status-summary-value">{{ overview?.protocolCompatibility?.activeRuleBindingCount ?? 0 }}</strong>
      </div>
      <div class="status-summary-card">
        <span class="status-summary-label">Protobuf 发包频率</span>
        <strong class="status-summary-value">{{ formatMessageRate(protobufTraffic?.total.messagesPerSecond) }}</strong>
      </div>
      <div class="status-summary-card">
        <span class="status-summary-label">Protobuf 累计发包</span>
        <strong class="status-summary-value">{{ protobufTraffic?.total.messageCount ?? 0 }}</strong>
      </div>
      <div class="status-summary-card">
        <span class="status-summary-label">Protobuf 累计字节</span>
        <strong class="status-summary-value">{{ formatBytes(protobufTraffic?.total.byteCount) }}</strong>
      </div>
      <div class="status-summary-card">
        <span class="status-summary-label">Protobuf 当前字节率</span>
        <strong class="status-summary-value">{{ formatByteRate(protobufTraffic?.total.bytesPerSecond) }}</strong>
      </div>
      <div class="status-summary-card">
        <span class="status-summary-label">snapshot_full</span>
        <strong class="status-summary-value">{{ snapshotSummary(protobufTraffic?.snapshotFull) }}</strong>
      </div>
    </section>

    <el-table
      :data="rows"
      border
      table-layout="fixed"
      class="admin-table"
      empty-text="暂无连接"
      row-key="actorId"
      :expand-row-keys="expandedKeys"
      @expand-change="handleExpandChange"
    >
      <el-table-column type="expand" width="44">
        <template #default="{ row }">
          <div v-if="expandedKeys.includes(row.actorId)" class="expanded-detail-grid">
            <div><span class="detail-key">显示名</span><span>{{ row.displayName || "-" }}</span></div>
            <div><span class="detail-key">房间</span><span>{{ row.roomCode || "-" }}</span></div>
            <div><span class="detail-key">协议版本</span><span>{{ row.protocolVersion || "-" }}</span></div>
            <div><span class="detail-key">协商版本</span><span>{{ row.negotiatedProtocolVersion || "-" }}</span></div>
            <div><span class="detail-key">兼容 Epoch</span><span>{{ row.compatibilityEpoch || "-" }}</span></div>
            <div><span class="detail-key">兼容规则</span><span>{{ activeCompatibilitySummary(row) }}</span></div>
            <div><span class="detail-key">程序版本</span><span>{{ row.programVersion || "-" }}</span></div>
            <div><span class="detail-key">远端地址</span><span>{{ row.remoteAddr || "-" }}</span></div>
            <div><span class="detail-key">连接 ID</span><span class="mono-text">{{ row.actorId || "-" }}</span></div>
            <template v-if="trafficFor(row)">
              <div><span class="detail-key">Protobuf 累计包</span><span>{{ trafficFor(row)?.total.messageCount }}</span></div>
              <div><span class="detail-key">Protobuf 当前频率</span><span>{{ formatMessageRate(trafficFor(row)?.total.messagesPerSecond) }}</span></div>
              <div><span class="detail-key">Protobuf 当前字节率</span><span>{{ formatByteRate(trafficFor(row)?.total.bytesPerSecond) }}</span></div>
              <div><span class="detail-key">snapshot_full</span><span>{{ snapshotSummary(trafficFor(row)?.snapshotFull) }}</span></div>
              <div><span class="detail-key">最大 Protobuf 包</span><span>{{ formatBytes(trafficFor(row)?.total.maxPacketBytes) }}</span></div>
              <div><span class="detail-key">最近 Protobuf 发送</span><span>{{ formatTimestamp(trafficFor(row)?.total.lastSentAt) }}</span></div>
            </template>
          </div>
          <el-table
            v-if="trafficFor(row)?.messageTypes.length"
            :data="trafficFor(row)?.messageTypes"
            size="small"
            border
            class="admin-table expanded-message-table"
          >
            <el-table-column prop="messageType" label="Protobuf 类型" min-width="190">
              <template #default="{ row: metric }"><span class="mono-text">{{ metric.messageType }}</span></template>
            </el-table-column>
            <el-table-column prop="messageCount" label="累计包" width="100" />
            <el-table-column label="当前频率" width="120">
              <template #default="{ row: metric }">{{ formatMessageRate(metric.messagesPerSecond) }}</template>
            </el-table-column>
            <el-table-column label="累计字节" width="120">
              <template #default="{ row: metric }">{{ formatBytes(metric.byteCount) }}</template>
            </el-table-column>
            <el-table-column label="最大单包" width="120">
              <template #default="{ row: metric }">{{ formatBytes(metric.maxPacketBytes) }}</template>
            </el-table-column>
            <el-table-column label="最近发送" min-width="180">
              <template #default="{ row: metric }">{{ formatTimestamp(metric.lastSentAt) }}</template>
            </el-table-column>
          </el-table>
        </template>
      </el-table-column>
      <el-table-column
        v-for="column in connectionColumns"
        :key="column.prop"
        :prop="column.prop"
        :label="column.label"
        :min-width="column.minWidth"
        :width="column.width"
        :show-overflow-tooltip="column.showOverflowTooltip"
        resizable
      >
        <template v-if="column.prop === 'channel'" #default="{ row }">
          {{ formatChannel(row.channel) }}
        </template>
        <template v-else-if="column.prop === 'compatibilityRuleCount'" #default="{ row }">
          {{ row.compatibilityRuleCount ?? 0 }}
        </template>
        <template v-else #default="{ row }">
          <span :class="{ 'mono-text': column.prop === 'actorId' }">{{ row[column.prop] || "-" }}</span>
        </template>
      </el-table-column>
      <el-table-column label="Protobuf 频率" width="130">
        <template #default="{ row }">{{ formatMessageRate(trafficFor(row)?.total.messagesPerSecond) }}</template>
      </el-table-column>
      <el-table-column label="snapshot_full" min-width="150">
        <template #default="{ row }">{{ snapshotSummary(trafficFor(row)?.snapshotFull) }}</template>
      </el-table-column>
    </el-table>
  </el-card>
</template>
