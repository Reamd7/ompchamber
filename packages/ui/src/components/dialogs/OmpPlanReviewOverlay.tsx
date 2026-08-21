/**
 * OmpPlanReviewOverlay — the plan-mode review surface (spec 02 §5.5 step 4).
 *
 * Mounted by the chat view alongside OmpDialogLayer for the active
 * (directory, sessionId). Opens when a plan proposal is pending:
 * - live: `omp.plan.review_requested` events via useOmpSessionStore;
 * - cold/reconnect: one authoritative `GET /api/omp/sessions/{id}/plan`
 *   seed while the store has no answer (events stay the live authority).
 *
 * The `xd://propose` tool result is held pending server-side until a
 * decision arrives, so the overlay is a blocking modal — dismiss requests
 * (Esc / outside click) are ignored, mirroring the TUI plan-review-overlay.
 * Decisions POST /api/omp/sessions/{id}/plan/review with the four TUI
 * choices (approve-execute / approve-compact / approve-keep / refine). The
 * plan body renders through the URI bridge (resolve `local://…`), with an
 * edit mode whose full text rides `editedContent` on the approve paths.
 *
 * Capability gating (R2): with modes.v1 off nothing renders and no request
 * fires — old-engine matrices keep the legacy plan surfaces untouched.
 */

import React from 'react';
import { toast } from '@/components/ui';
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
import { useRuntimeAPIs } from '@/hooks/useRuntimeAPIs';
import { useOmpFeatureEnabled } from '@/hooks/useOmpFeatureEnabled';
import type { OmpPlanReviewChoice, OmpPlanReviewDetails } from '@/lib/api/omp';
import { useOmpModeState, useOmpPlanReview } from '@/sync/useOmpSessionStore';
import { cn } from '@/lib/utils';

interface OmpPlanReviewOverlayProps {
  directory: string | undefined;
  sessionId: string | undefined;
}

type PendingReview = { details: OmpPlanReviewDetails; requestedAt: number };

const isPlanishMode = (mode: string | undefined): boolean =>
  mode === 'plan' || mode === 'plan_paused';

export const OmpPlanReviewOverlay: React.FC<OmpPlanReviewOverlayProps> = ({ directory, sessionId }) => {
  const { t } = useI18n();
  const modesEnabled = useOmpFeatureEnabled('modes.v1');
  const { ompModes, ompUri } = useRuntimeAPIs();
  const storeReview = useOmpPlanReview(directory ?? '', sessionId);
  const modeState = useOmpModeState(directory ?? '', sessionId);

  const [seed, setSeed] = React.useState<PendingReview | null>(null);
  const seededKeyRef = React.useRef<string | null>(null);
  const [planContent, setPlanContent] = React.useState<string | null>(null);
  const [contentState, setContentState] = React.useState<'loading' | 'ready' | 'unavailable'>('loading');
  const [editedContent, setEditedContent] = React.useState<string | null>(null);
  const [refineOpen, setRefineOpen] = React.useState(false);
  const [feedback, setFeedback] = React.useState('');
  const [submitting, setSubmitting] = React.useState<OmpPlanReviewChoice | null>(null);

  const active = modesEnabled && typeof directory === 'string' && directory.length > 0
    && typeof sessionId === 'string' && sessionId.length > 0;

  // An authoritative mode answer outside plan* suppresses any stale pending
  // review (approve exits the mode; the bridge clears with it).
  const modeAllowsReview = modeState === null || isPlanishMode(modeState.mode);
  const pending: PendingReview | null = !active || !modeAllowsReview
    ? null
    : (storeReview ?? seed);

  // Cold/reconnect seed: exactly one GET /plan per (directory, session) while
  // the event stream has not delivered a review.
  React.useEffect(() => {
    if (!active || storeReview !== null) return;
    const key = `${directory}::${sessionId}`;
    if (seededKeyRef.current === key) return;
    seededKeyRef.current = key;
    let cancelled = false;
    void ompModes.getPlan(sessionId, { directory }).then((result) => {
      if (cancelled) return;
      setSeed(result.ok && result.data.review
        ? { details: result.data.review, requestedAt: 0 }
        : null);
    });
    return () => {
      cancelled = true;
    };
  }, [ompModes, active, directory, sessionId, storeReview]);

  React.useEffect(() => {
    if (!active) {
      seededKeyRef.current = null;
    }
  }, [active]);

  // Plan body through the URI bridge; failures degrade to a note — the
  // decision endpoints do not depend on the rendered content.
  React.useEffect(() => {
    if (pending === null) {
      setPlanContent(null);
      setContentState('loading');
      setEditedContent(null);
      setRefineOpen(false);
      return;
    }
    if (!directory || !sessionId) return;
    let cancelled = false;
    setContentState('loading');
    void ompUri.resolve({ url: pending.details.planFilePath, sessionID: sessionId, directory }).then((result) => {
      if (cancelled) return;
      if (result.ok) {
        setPlanContent(result.resource.content);
        setContentState('ready');
      } else {
        setPlanContent(null);
        setContentState('unavailable');
      }
    });
    return () => {
      cancelled = true;
    };
  }, [ompUri, pending, directory, sessionId]);

  const submit = React.useCallback((choice: OmpPlanReviewChoice) => {
    if (submitting !== null || !directory || !sessionId) return;
    setSubmitting(choice);
    void ompModes.submitPlanReview(sessionId, {
      directory,
      choice,
      ...(choice === 'refine' && feedback.trim() ? { feedback: feedback.trim() } : {}),
      ...(choice !== 'refine' && editedContent !== null ? { editedContent } : {}),
    }).then((result) => {
      setSubmitting(null);
      if (result.ok) {
        // The decision settles the pending propose server-side; the store
        // reconciles through omp.mode.changed / a fresh review_requested.
        setSeed(null);
        seededKeyRef.current = null;
        if (choice === 'refine') {
          toast.info(t('dialogs.omp.planReview.refineSubmitted'));
        }
        return;
      }
      if (!result.ok && !result.unavailable && result.reason === 'no-pending-proposal') {
        // The proposal is gone (superseded/aborted) — nothing left to decide.
        setSeed(null);
        seededKeyRef.current = null;
        return;
      }
      toast.error(result.unavailable
        ? t('dialogs.omp.planReview.unavailable')
        : t('dialogs.omp.planReview.decisionFailed'));
    });
  }, [directory, feedback, ompModes, sessionId, submitting, t, editedContent]);

  if (pending === null) return null;

  const editing = editedContent !== null;
  const busy = submitting !== null;

  const choiceLabel = (choice: OmpPlanReviewChoice): string => {
    switch (choice) {
      case 'approve-execute': return t('dialogs.omp.planReview.choice.approveExecute');
      case 'approve-compact': return t('dialogs.omp.planReview.choice.approveCompact');
      case 'approve-keep': return t('dialogs.omp.planReview.choice.approveKeep');
      case 'refine': return t('dialogs.omp.planReview.choice.refine');
    }
  };

  return (
    <Dialog
      open
      onOpenChange={(nextOpen) => {
        // Blocking modal: the propose turn is held pending server-side — a
        // stray dismiss must not strand it without a decision.
        if (nextOpen) return;
      }}
    >
      <DialogContent
        showCloseButton={false}
        className="max-w-3xl"
        aria-describedby={undefined}
        data-testid="omp-plan-review-overlay"
      >
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Icon name="survey" className="size-4 flex-shrink-0 text-primary" />
            <span className="min-w-0 truncate">{pending.details.title}</span>
          </DialogTitle>
          <DialogDescription className="font-mono typography-meta">{pending.details.planFilePath}</DialogDescription>
        </DialogHeader>

        <div className="flex min-h-0 flex-col gap-3">
          <div className="flex items-center justify-between gap-2">
            <span className="typography-meta text-muted-foreground">
              {pending.details.planExists
                ? t('dialogs.omp.planReview.planReady')
                : t('dialogs.omp.planReview.planMissing')}
            </span>
            {contentState === 'ready' ? (
              <Button
                type="button"
                variant="ghost"
                size="xs"
                disabled={busy}
                aria-pressed={editing || undefined}
                onClick={() => setEditedContent(editing ? null : (planContent ?? ''))}
              >
                {editing ? t('dialogs.omp.planReview.previewMode') : t('dialogs.omp.planReview.editMode')}
              </Button>
            ) : null}
          </div>

          {editing ? (
            <textarea
              value={editedContent}
              onChange={(event) => setEditedContent(event.target.value)}
              disabled={busy}
              spellCheck={false}
              aria-label={t('dialogs.omp.planReview.editAreaLabel')}
              className="typography-meta h-[min(48vh,480px)] w-full resize-none rounded-md border border-border bg-background p-3 font-mono focus:outline-none focus-visible:ring-1 focus-visible:ring-primary"
            />
          ) : contentState === 'loading' ? (
            <div className="flex h-40 items-center justify-center rounded-md border border-border/40" aria-live="polite">
              <span className="typography-meta text-muted-foreground">{t('dialogs.omp.planReview.loadingPlan')}</span>
            </div>
          ) : contentState === 'unavailable' ? (
            <div className="flex h-40 items-center justify-center rounded-md border border-border/40 px-6 text-center" aria-live="polite">
              <span className="typography-meta text-muted-foreground">{t('dialogs.omp.planReview.contentUnavailable')}</span>
            </div>
          ) : (
            <div
              className={cn(
                'h-[min(48vh,480px)] overflow-y-auto rounded-md border border-border/40 p-4',
                'typography-body whitespace-pre-wrap break-words',
              )}
            >
              {planContent ?? ''}
            </div>
          )}

          {refineOpen ? (
            <div className="flex flex-col gap-2">
              <textarea
                value={feedback}
                onChange={(event) => setFeedback(event.target.value)}
                disabled={busy}
                rows={3}
                placeholder={t('dialogs.omp.planReview.feedbackPlaceholder')}
                aria-label={t('dialogs.omp.planReview.feedbackLabel')}
                className="typography-meta w-full resize-none rounded-md border border-border bg-background p-2 focus:outline-none focus-visible:ring-1 focus-visible:ring-primary"
              />
              <div className="flex flex-wrap items-center justify-end gap-2">
                <Button
                  type="button"
                  variant="ghost"
                  size="sm"
                  disabled={busy}
                  onClick={() => setRefineOpen(false)}
                >
                  {t('dialogs.omp.planReview.refineCancel')}
                </Button>
                <Button
                  type="button"
                  variant="secondary"
                  size="sm"
                  disabled={busy || !feedback.trim()}
                  onClick={() => submit('refine')}
                >
                  {submitting === 'refine' ? t('dialogs.omp.planReview.submitting') : t('dialogs.omp.planReview.refineSubmit')}
                </Button>
              </div>
            </div>
          ) : null}
        </div>

        <div className="flex flex-wrap items-center justify-end gap-2">
          {refineOpen ? null : (
            <Button
              type="button"
              variant="secondary"
              size="sm"
              disabled={busy}
              onClick={() => setRefineOpen(true)}
            >
              {choiceLabel('refine')}
            </Button>
          )}
          <Button
            type="button"
            variant="outline"
            size="sm"
            disabled={busy}
            onClick={() => submit('approve-keep')}
          >
            {submitting === 'approve-keep' ? t('dialogs.omp.planReview.submitting') : choiceLabel('approve-keep')}
          </Button>
          <Button
            type="button"
            variant="outline"
            size="sm"
            disabled={busy}
            onClick={() => submit('approve-compact')}
          >
            {submitting === 'approve-compact' ? t('dialogs.omp.planReview.submitting') : choiceLabel('approve-compact')}
          </Button>
          <Button
            type="button"
            size="sm"
            disabled={busy}
            onClick={() => submit('approve-execute')}
          >
            {submitting === 'approve-execute' ? t('dialogs.omp.planReview.submitting') : choiceLabel('approve-execute')}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
};
