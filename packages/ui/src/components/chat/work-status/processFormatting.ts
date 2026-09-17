import type { OmpProcessEntry } from '@/lib/api/omp';
import type { I18nKey } from '@/lib/i18n/messages/en';

export const formatProcessBytes = (bytes: number): string => {
  if (!Number.isFinite(bytes) || bytes < 0) return '';
  if (bytes < 1024) return `${Math.round(bytes)} B`;
  const units = ['KB', 'MB', 'GB', 'TB'] as const;
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value >= 100 ? Math.round(value) : value.toFixed(1)} ${units[unit]}`;
};

export const formatProcessCpu = (percent: number): string =>
  `${percent < 10 ? percent.toFixed(1) : Math.round(percent)}%`;

export const processStatusLabelKey = {
  running: 'chat.workStatus.processes.running',
  exited: 'chat.workStatus.processes.exited',
  killed: 'chat.workStatus.processes.killed',
  failed: 'chat.workStatus.processes.failed',
} satisfies Record<OmpProcessEntry['status'], I18nKey>;
