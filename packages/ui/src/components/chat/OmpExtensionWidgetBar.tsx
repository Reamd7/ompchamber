/**
 * OmpExtensionWidgetBar — extension chrome widget strip (spec 09 §5.1).
 *
 * Renders the string[] widgets an omp extension pushes through the host
 * dialog bridge's `setWidget` (mirroring RpcExtensionUIRequest). Content is
 * verbatim by contract: the extension's rows are the TUI checksum anchor —
 * no reformatting, no markdown, no translation. Above/below placement maps
 * to the composer's top/bottom edges (SDK `aboveEditor`/`belowEditor`).
 *
 * Empty chrome renders nothing (data presence is the capability gate: the
 * server only produces widgets when `extensionChrome.v1` is on).
 */
import React from 'react';

import { useI18n } from '@/lib/i18n';
import { useOmpChromeState } from '@/sync/useOmpSessionStore';
import type { OmpChromeWidget } from '@/sync/omp-event-reducer';

interface OmpExtensionWidgetBarProps {
  directory: string;
  placement: 'aboveEditor' | 'belowEditor';
}

const widgetSortKey = (widget: OmpChromeWidget): string =>
  `${String(widget.updatedAt).padStart(13, '0')}:${widget.key}`;

export const OmpExtensionWidgetBar: React.FC<OmpExtensionWidgetBarProps> = ({ directory, placement }) => {
  const { t } = useI18n();
  const chrome = useOmpChromeState(directory);

  const widgets = React.useMemo(
    () =>
      Object.values(chrome.widgets)
        .filter((widget) => (widget.placement ?? 'aboveEditor') === placement && widget.lines.length > 0)
        .sort((a, b) => widgetSortKey(a).localeCompare(widgetSortKey(b))),
    [chrome.widgets, placement],
  );

  if (widgets.length === 0) return null;

  return (
    <div
      role="region"
      aria-label={t('chat.extensionWidgets.ariaLabel')}
      data-testid="omp-extension-widget-bar"
      className="pointer-events-none flex flex-col gap-0.5 px-3 py-1"
    >
      {widgets.map((widget) => (
        <div
          key={widget.key}
          className="overflow-x-auto whitespace-pre font-mono typography-meta text-muted-foreground"
        >
          {widget.lines.map((line, index) => (
            <div key={index}>{line}</div>
          ))}
        </div>
      ))}
    </div>
  );
};
