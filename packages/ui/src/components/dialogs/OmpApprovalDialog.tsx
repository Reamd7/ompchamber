/**
 * OmpApprovalDialog — the omp approval modal body (spec 03 §5.3.1/§5.3.2).
 *
 * Button contract is exactly Approve/Deny (wrapper.ts hardcodes
 * `select(safetyPrompt, ["Approve","Deny"])`); the body renders the server's
 * `approval.prompt` verbatim — the TUI-parity checksum anchor. "Always
 * allow" lives in the overflow menu as an advanced action and is a strict
 * ordered transaction: the settings write must succeed before the Approve
 * respond is sent (R10); a failed write keeps the dialog open with nothing
 * approved.
 */

import React from 'react';
import { Button } from '@/components/ui/button';
import { Icon } from '@/components/icon/Icon';
import { useI18n } from '@/lib/i18n';
import { toast } from '@/components/ui';
import type { OmpPendingDialog } from '@/lib/api/omp';
import { ompDialogController } from '@/sync/omp-dialog-controller';
import { useOmpDialogUi } from '@/sync/useOmpDialogStore';
import { cn } from '@/lib/utils';

interface OmpApprovalDialogProps {
  directory: string;
  dialog: Extract<OmpPendingDialog, { kind: 'approval' }>;
}

export const OmpApprovalDialog: React.FC<OmpApprovalDialogProps> = ({ directory, dialog }) => {
  const { t } = useI18n();
  const ui = useOmpDialogUi(directory, dialog.id);
  const [confirmingAlways, setConfirmingAlways] = React.useState(false);
  const [alwaysBusy, setAlwaysBusy] = React.useState(false);
  const inflight = ui?.respondInflight === true;
  const toolName = dialog.approval.toolName;

  const respond = React.useCallback(async (approve: boolean) => {
    await ompDialogController.respond(directory, dialog.id, {
      kind: 'select',
      value: approve ? 'Approve' : 'Deny',
    });
  }, [dialog.id, directory]);

  const runAlwaysAllow = React.useCallback(async () => {
    if (!toolName) return;
    setAlwaysBusy(true);
    const result = await ompDialogController.alwaysAllowAndApprove(directory, dialog.id, toolName);
    setAlwaysBusy(false);
    setConfirmingAlways(false);
    if (!result.ok) {
      const conflict = 'conflict' in result ? result.conflict === true : false;
      const unavailable = 'unavailable' in result ? result.unavailable === true : false;
      if (!conflict && !unavailable) toast.error(t('dialogs.omp.approval.alwaysAllowFailed'));
    }
  }, [dialog.id, directory, t, toolName]);

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center gap-2">
        {toolName ? (
          <span className="inline-flex items-center gap-1 rounded border border-border/40 px-1.5 py-0.5 typography-meta text-muted-foreground">
            <Icon name="terminal-box" className="size-3.5" />
            <span className="font-mono">{toolName}</span>
          </span>
        ) : null}
        {dialog.approval.tier ? (
          <span className="rounded border border-border/40 px-1.5 py-0.5 typography-meta text-muted-foreground">
            {dialog.approval.tier}
          </span>
        ) : null}
        {dialog.approval.approvalMode ? (
          <span className="rounded border border-border/40 px-1.5 py-0.5 typography-meta text-muted-foreground">
            {dialog.approval.approvalMode}
          </span>
        ) : null}
      </div>

      <div
        className="max-h-[45vh] overflow-y-auto whitespace-pre-wrap break-words rounded border border-border/30 bg-muted/20 p-3 font-mono text-[13px] leading-relaxed text-foreground"
        aria-label={t('dialogs.omp.approval.bodyAria')}
      >
        {dialog.approval.prompt}
      </div>

      {ui?.respondError ? (
        <p className="text-status-error typography-meta" role="alert">
          {t('dialogs.omp.approval.respondFailed')}: {ui.respondError}
        </p>
      ) : null}

      {confirmingAlways && toolName ? (
        <div className="rounded border border-border/40 p-3 flex flex-col gap-2">
          <p className="typography-meta text-muted-foreground">
            {t('dialogs.omp.approval.alwaysAllowConfirmBody', { tool: toolName })}
          </p>
          <div className="flex justify-end gap-2">
            <Button variant="ghost" size="sm" disabled={alwaysBusy || inflight} onClick={() => setConfirmingAlways(false)}>
              {t('dialogs.omp.approval.alwaysAllowCancel')}
            </Button>
            <Button variant="outline" size="sm" disabled={alwaysBusy || inflight} onClick={() => void runAlwaysAllow()}>
              {alwaysBusy ? <Icon name="loader-4" className="size-4 animate-spin" /> : null}
              {t('dialogs.omp.approval.alwaysAllowConfirm')}
            </Button>
          </div>
        </div>
      ) : null}

      <div className="flex items-center justify-between gap-2">
        <div className="flex items-center gap-1">
          {toolName && !confirmingAlways ? (
            <Button
              variant="ghost"
              size="sm"
              disabled={inflight || alwaysBusy}
              aria-label={t('dialogs.omp.approval.alwaysAllow')}
              title={t('dialogs.omp.approval.alwaysAllow')}
              onClick={() => setConfirmingAlways(true)}
            >
              <Icon name="more" className="size-4" />
            </Button>
          ) : null}
        </div>
        <div className="flex items-center gap-2">
          <Button
            variant="outline"
            disabled={inflight || alwaysBusy}
            className={cn('min-w-24')}
            onClick={() => void respond(false)}
          >
            {inflight ? <Icon name="loader-4" className="size-4 animate-spin" /> : null}
            {t('dialogs.omp.approval.deny')}
          </Button>
          <Button disabled={inflight || alwaysBusy} className="min-w-24" onClick={() => void respond(true)}>
            {inflight ? <Icon name="loader-4" className="size-4 animate-spin" /> : null}
            {t('dialogs.omp.approval.approve')}
          </Button>
        </div>
      </div>
    </div>
  );
};
