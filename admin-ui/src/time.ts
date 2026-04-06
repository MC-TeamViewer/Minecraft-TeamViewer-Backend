import type { TrafficGranularity } from "@/types";

function pad2(value: number): string {
  return String(value).padStart(2, "0");
}

function parseLocalDateParts(value: string): Date | null {
  const match = /^(\d{4})-(\d{2})-(\d{2})$/.exec(value.trim());
  if (!match) {
    return null;
  }
  const [, year, month, day] = match;
  return new Date(Number(year), Number(month) - 1, Number(day), 0, 0, 0, 0);
}

function parseLocalDateTimeParts(value: string): Date | null {
  const match = /^(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2})(?::(\d{2}))?$/.exec(value.trim());
  if (!match) {
    return null;
  }
  const [, year, month, day, hour, minute, second] = match;
  return new Date(
    Number(year),
    Number(month) - 1,
    Number(day),
    Number(hour),
    Number(minute),
    Number(second ?? "0"),
    0,
  );
}

export function formatLocalDate(value: Date): string {
  return `${value.getFullYear()}-${pad2(value.getMonth() + 1)}-${pad2(value.getDate())}`;
}

export function formatLocalDateTime(value: Date): string {
  return `${formatLocalDate(value)}T${pad2(value.getHours())}:${pad2(value.getMinutes())}:${pad2(value.getSeconds())}`;
}

export function normalizeDateInput(value: string | null | undefined): string {
  if (!value) {
    return "";
  }
  const parsed = parseLocalDateParts(value);
  return parsed ? formatLocalDate(parsed) : "";
}

export function alignHourlyDateTime(value: string | null | undefined): string {
  if (!value) {
    return "";
  }
  const parsed = parseLocalDateTimeParts(value);
  if (!parsed) {
    return "";
  }
  parsed.setMinutes(0, 0, 0);
  return formatLocalDateTime(parsed);
}

export function alignTrafficDateTime(
  value: string | null | undefined,
  granularity: TrafficGranularity,
): string {
  if (!value) {
    return "";
  }
  const parsed = parseLocalDateTimeParts(value);
  if (!parsed) {
    return "";
  }
  if (granularity === "1d") {
    parsed.setHours(0, 0, 0, 0);
    return formatLocalDateTime(parsed);
  }
  if (granularity === "1h") {
    parsed.setMinutes(0, 0, 0);
    return formatLocalDateTime(parsed);
  }
  const bucketMinutes = granularity === "15m" ? 15 : granularity === "5m" ? 5 : 1;
  parsed.setMinutes(Math.floor(parsed.getMinutes() / bucketMinutes) * bucketMinutes, 0, 0);
  return formatLocalDateTime(parsed);
}

export function formatDisplayDate(value: string | null | undefined): string {
  if (!value) {
    return "-";
  }
  const normalized = normalizeDateInput(value);
  return normalized || value;
}

export function formatDisplayDateTime(value: string | null | undefined): string {
  if (!value) {
    return "-";
  }
  const parsed = parseLocalDateTimeParts(value);
  return (parsed ? formatLocalDateTime(parsed) : value).replace("T", " ").slice(0, 16);
}
