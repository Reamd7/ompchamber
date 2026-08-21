/**
 * OmpDialogLayer — the per-session dialog queue surface (spec 03 §5.6.3).
 *
 * Mounted once by the chat view for (directory, sessionId). Owns three
 * things and nothing else:
 * 1. The UI attachment lease (R13): acquire on mount / sessionId change,
 *    heartbeat at the server-advised interval, release on unmount — the
 *    engine's `hasUI` is exactly this lease's holder count.
 * 2. The presented-ack anchor (§5.4.3): exactly one ack per activation of
 *    the queue FRONT (the active modal); buried dialogs stay un-acked until
 *    they surface, so T_answer never starts for an invisible dialog.
 * 3. The queue rendering: front = active modal (kind-dispatched), the rest
 *    surfaced as a count badge. Background sessions are notified by the
 *    sync layer (C9), not here.
 *
 * Capability gating (R2): with `dialogs.v1` off the layer renders nothing
 * and holds no lease — old-engine matrices degrade to wire-only behavior.
 */

import React from 'react';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import { Icon } from '@/components/icon/Icon';
import { useI18n } from '@/lib/i18n';
import { createOmpDialogsAPI, type OmpPendingDialog } from '@/lib/api/omp';
import { useOmpFeatureEnabled } from '@/hooks/useOmpFeatureEnabled';
import { OmpDialogLease, getOmpDialogClientId } from '@/sync/omp-dialog-lease';
import { ompDialogController } from '@/sync/omp-dialog-controller';
import { useOmpDialogStore, useOmpDialogsForSession } from '@/sync/useOmpDialogStore';
import { getRuntimeKey } from '@/lib/runtime-switch';
import { OmpApprovalDialog } from './OmpApprovalDialog';
import { OmpAskDialogModal } from './OmpAskDialogModal';

interface OmpDialogLayerProps {
  directory: string | undefined;
  sessionId: string | undefined;
}

const leaseApi = createOmpDialogsAPI();

/** Simple generic bodies for the non-approval/ask kinds the bridge emits. */
const SelectBody: React.FC<{ dialog: Extract<OmpPendingDialog, { kind: 'select' }>; onRespond: (value?: string) => void }> = ({ dialog, onRespond }) => {
  const { t } = useI18n();
  return (
    <div className="flex flex-col gap-2">
      <p className="text-sm text-foreground">{dialog.select.title}</p>
      <div className="flex flex-col gap-1.5">
        {dialog.select.options.map((option) => (
          <Button key={option} variant="outline" className="justify-start" onClick={() => onRespond(option)}>
            {option}
          </Button>
        ))}
      </div>
      <Button variant="ghost" size="sm" className="self-start" onClick={() => onRespond()}>
        {t('dialogs.omp.generic.cancel')}
      </Button>
    </div>
  );
};

const ConfirmBody: React.FC<{ dialog: Extract<OmpPendingDialog, { kind: 'confirm' }>; onRespond: (value?: boolean) => void }> = ({ dialog, onRespond }) => {
  const { t } = useI18n();
  return (
    <div className="flex flex-col gap-3">
      <p className="text-sm font-medium text-foreground">{dialog.confirm.title}</p>
      {dialog.confirm.message ? <p className="whitespace-pre-wrap text-sm text-muted-foreground">{dialog.confirm.message}</p> : null}
      <div className="flex justify-end gap-2">
        <Button variant="ghost" onClick={() => onRespond()}>
          {t('dialogs.omp.generic.cancel')}
        </Button>
        <Button variant="outline" onClick={() => onRespond(false)}>
          {t('dialogs.omp.confirm.no')}
        </Button>
        <Button onClick={() => onRespond(true)}>
          {t('dialogs.omp.confirm.yes')}
        </Button>
      </div>
    </div>
  );
};

const InputBody: React.FC<{
  dialog: Extract<OmpPendingDialog, { kind: 'input' | 'editor' }>;
  onRespond: (kind: 'input' | 'editor', value?: string) => void;
}> = ({ dialog, onRespond }) => {
  const { t } = useI18n();
  const kind = dialog.kind;
  const title = kind === 'input' ? dialog.input.title : dialog.editor.title;
  const placeholder = kind === 'input' ? dialog.input.placeholder : dialog.editor.placeholder;
  const [value, setValue] = React.useState(placeholder ?? '');
  return (
    <div className="flex flex-col gap-3">
      <p className="text-sm font-medium text-foreground">{title}</p>
      <textarea
        value={value}
        onChange={(event) => setValue(event.target.value)}
        rows={4}
        className="w-full resize-none rounded border border-border/30 bg-transparent px-2 py-1 text-sm text-foreground outline-none transition-colors focus:border-primary"
      />
      <div className="flex justify-end gap-2">
        <Button variant="ghost" onClick={() => onRespond(kind)}>
          {t('dialogs.omp.generic.cancel')}
        </Button>
        <Button onClick={() => onRespond(kind, value)}>{t('dialogs.omp.input.submit')}</Button>
      </div>
    </div>
  );
};

export const OmpDialogLayer: React.FC<OmpDialogLayerProps> = ({ directory, sessionId }) => {
  const { t } = useI18n();
  const dialogsEnabled = useOmpFeatureEnabled('dialogs.v1');
  const dialogs = useOmpDialogsForSession(directory ?? '', sessionId);
  const active = dialogs.length > 0 ? dialogs[0] : null;

  // Lease lifecycle: one per (directory, sessionId) while the surface is
  // capability-enabled and a session is open. Re-acquire on change; release
  // on cleanup. The holder TTL covers the swap window server-side.
  React.useEffect(() => {
    if (!dialogsEnabled || !sessionId || !directory) return;
    // Do not depend on sibling SyncProvider effect ordering. A session can
    // mount this layer before the omp event pipeline adopts the runtime;
    // reconcile would otherwise discard its authoritative snapshot.
    useOmpDialogStore.getState().adoptRuntime(getRuntimeKey());
    const lease = new OmpDialogLease({
      api: leaseApi,
      directory,
      sessionId,
      clientId: getOmpDialogClientId(),
      onActive: () => ompDialogController.reconcile(directory),
    });
    lease.start();
    return () => {
      lease.release();
    };
  }, [dialogsEnabled, directory, sessionId]);
  // presented-ack: exactly once per activation of the queue front.
  React.useEffect(() => {
    if (active === null || !dialogsEnabled || !directory) return;
    void ompDialogController.presented(directory, active.id);
  }, [active?.id, dialogsEnabled, directory]); // eslint-disable-line react-hooks/exhaustive-deps

  if (!dialogsEnabled || !directory || active === null) return null;

  const titleFor = (dialog: OmpPendingDialog): string => {
    switch (dialog.kind) {
      case 'approval': return t('dialogs.omp.approval.title');
      case 'ask': return t('dialogs.omp.ask.title');
      case 'select': return t('dialogs.omp.select.title');
      case 'confirm': return dialog.confirm.title;
      case 'input': return dialog.input.title;
      case 'editor': return dialog.editor.title;
    }
  };

  return (
    <Dialog
      open
      onOpenChange={(nextOpen) => {
        // Controlled-open modal: dismiss requests (Esc / outside) reach us
        // but never close anything by themselves. Approval ignores both —
        // a stray keypress must not answer a tool approval. Everything
        // else maps a dismiss onto the tool's cancel contract.
        if (nextOpen) return;
        if (active.kind === 'approval') return;
        void ompDialogController.respond(directory ?? '', active.id, { kind: 'cancel' });
      }}
    >
      <DialogContent
        showCloseButton={false}
        aria-describedby={undefined}
      >
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Icon name="error-warning" className="size-4 text-status-warning" />
            {titleFor(active)}
            {dialogs.length > 1 ? (
              <span className="ml-auto rounded-full border border-border/40 px-2 py-0.5 typography-meta text-muted-foreground">
                {t('dialogs.omp.queue.moreWaiting', { count: dialogs.length - 1 })}
              </span>
            ) : null}
          </DialogTitle>
          <DialogDescription className="sr-only">{titleFor(active)}</DialogDescription>
        </DialogHeader>
        {active.kind === 'approval' ? (
          <OmpApprovalDialog directory={directory} dialog={active} />
        ) : active.kind === 'ask' ? (
          <OmpAskDialogModal directory={directory} dialog={active} />
        ) : active.kind === 'select' ? (
          <SelectBody
            dialog={active}
            onRespond={(value) => void ompDialogController.respond(directory, active.id, value === undefined ? { kind: 'cancel' } : { kind: 'select', value })}
          />
        ) : active.kind === 'confirm' ? (
          <ConfirmBody
            dialog={active}
            onRespond={(value) => void ompDialogController.respond(directory, active.id, value === undefined ? { kind: 'cancel' } : { kind: 'confirm', value })}
          />
        ) : (
          <InputBody
            dialog={active}
            onRespond={(kind, value) => void ompDialogController.respond(directory, active.id, value === undefined ? { kind: 'cancel' } : { kind, value })}
          />
        )}
      </DialogContent>
    </Dialog>
  );
};

