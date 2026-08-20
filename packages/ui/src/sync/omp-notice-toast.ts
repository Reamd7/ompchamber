/**
 * omp notice toasts — three-level instant toasts driven by
 * `omp.notice.raised` (spec 05 §5.1 row 10; permission-toast pattern).
 *
 * Notices are transient by design: they never feed the notification store
 * or system notifications — those are owned by terminal agent_end wire
 * state (spec 08 §5.6 M9 authority rule). Dedup key = (level, source,
 * message) so a repeated notice updates its toast instead of stacking.
 */

import { toast } from '@/components/ui';
import { formatMessage, useI18nStore } from '@/lib/i18n/store';
import type { OmpEventEffect } from './omp-event-reducer';

type NoticeEffect = Extract<OmpEventEffect, { kind: 'notice' }>;

const TITLE_KEY_BY_LEVEL: Record<NoticeEffect['level'], Parameters<typeof formatMessage>[1]> = {
  info: 'toast.ompNotice.title.info',
  warning: 'toast.ompNotice.title.warning',
  error: 'toast.ompNotice.title.error',
}

const showToastByLevel: Record<NoticeEffect['level'], (title: string, options: { id: string; description: string }) => void> = {
  info: (title, options) => toast.info(title, options),
  warning: (title, options) => toast.warning(title, options),
  error: (title, options) => toast.error(title, options),
};

export const getOmpNoticeToastId = (effect: NoticeEffect): string =>
  `omp-notice:${effect.level}:${effect.source ?? ''}:${effect.message}`;

/**
 * Shows one notice toast. Returns false when the payload carries no visible
 * message (caller still consumed the event).
 */
export const showOmpNoticeToast = (effect: NoticeEffect): boolean => {
  const message = effect.message.trim();
  if (message.length === 0) return false;
  const dictionary = useI18nStore.getState().dictionary;
  showToastByLevel[effect.level](
    formatMessage(dictionary, TITLE_KEY_BY_LEVEL[effect.level]),
    { id: getOmpNoticeToastId(effect), description: message },
  );
  return true;
};
