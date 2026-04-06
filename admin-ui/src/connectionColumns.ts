export interface ConnectionColumn {
  prop: string;
  label: string;
  minWidth: number;
  width?: number;
  showOverflowTooltip?: boolean;
}

export const connectionColumns: ConnectionColumn[] = [
  { prop: "channel", label: "连接类型", minWidth: 110, width: 110 },
  { prop: "displayName", label: "显示名", minWidth: 180, showOverflowTooltip: true },
  { prop: "roomCode", label: "房间", minWidth: 140, showOverflowTooltip: true },
  { prop: "protocolVersion", label: "协议版本", minWidth: 120, showOverflowTooltip: true },
  { prop: "programVersion", label: "程序版本", minWidth: 220, showOverflowTooltip: true },
  { prop: "remoteAddr", label: "远端地址", minWidth: 180, showOverflowTooltip: true },
  { prop: "actorId", label: "连接 ID", minWidth: 220, showOverflowTooltip: true },
];
