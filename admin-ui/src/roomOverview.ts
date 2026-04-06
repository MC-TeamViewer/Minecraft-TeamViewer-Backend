export function summarizeRoomIds(ids: string[] | undefined): string {
  const count = ids?.length ?? 0;
  return count > 0 ? `${count} 个` : "-";
}
