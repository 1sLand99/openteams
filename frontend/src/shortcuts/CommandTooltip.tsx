import React, {
  Children,
  cloneElement,
  type ReactElement,
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
} from 'react';
import { createPortal } from 'react-dom';
import { useCommandPresentation } from './ShortcutProvider';

type Props = {
  commandId: string;
  children: ReactElement<Record<string, unknown>>;
  side?: 'top' | 'bottom';
  align?: 'center' | 'start' | 'end';
};

const TOOLTIP_HOVER_DELAY_MS = 1_200;
const TOOLTIP_VIEWPORT_MARGIN_PX = 8;
const TOOLTIP_GAP_PX = 8;

type TooltipPosition = {
  left: number;
  top: number;
};

export function CommandTooltip({
  commandId,
  children,
  side = 'top',
  align = 'center',
}: Props) {
  const presentation = useCommandPresentation(commandId);
  const [open, setOpen] = useState(false);
  const [position, setPosition] = useState<TooltipPosition | null>(null);
  const anchorRef = useRef<HTMLSpanElement>(null);
  const tooltipRef = useRef<HTMLSpanElement>(null);
  const hoverTimeoutRef = useRef<number | null>(null);
  const tooltipId = useId();
  const child = Children.only(children) as ReactElement<Record<string, unknown>>;
  const existingDescribedBy =
    typeof child.props['aria-describedby'] === 'string'
      ? child.props['aria-describedby']
      : undefined;
  const describedBy = [existingDescribedBy, tooltipId].filter(Boolean).join(' ');
  const disabledButton = child.type === 'button' && child.props.disabled === true;
  const triggerProps = {
    'aria-describedby': describedBy,
    'aria-keyshortcuts': presentation.ariaKeyShortcuts || undefined,
  };

  const clearHoverTimeout = useCallback(() => {
    if (hoverTimeoutRef.current !== null) {
      window.clearTimeout(hoverTimeoutRef.current);
      hoverTimeoutRef.current = null;
    }
  }, []);

  useEffect(() => clearHoverTimeout, [clearHoverTimeout]);

  useLayoutEffect(() => {
    if (!open || !anchorRef.current || !tooltipRef.current) return;

    const anchorRect = anchorRef.current.getBoundingClientRect();
    const tooltipRect = tooltipRef.current.getBoundingClientRect();
    const preferredLeft =
      align === 'start'
        ? anchorRect.left
        : align === 'end'
          ? anchorRect.right - tooltipRect.width
          : anchorRect.left + (anchorRect.width - tooltipRect.width) / 2;
    const maxLeft =
      window.innerWidth - TOOLTIP_VIEWPORT_MARGIN_PX - tooltipRect.width;
    const left = Math.max(
      TOOLTIP_VIEWPORT_MARGIN_PX,
      Math.min(maxLeft, preferredLeft),
    );
    const preferredTop =
      side === 'bottom'
        ? anchorRect.bottom + TOOLTIP_GAP_PX
        : anchorRect.top - TOOLTIP_GAP_PX - tooltipRect.height;
    const maxTop =
      window.innerHeight - TOOLTIP_VIEWPORT_MARGIN_PX - tooltipRect.height;
    const top = Math.max(
      TOOLTIP_VIEWPORT_MARGIN_PX,
      Math.min(maxTop, preferredTop),
    );

    setPosition({ left, top });
  }, [align, open, side]);

  const handlePointerEnter = () => {
    clearHoverTimeout();
    hoverTimeoutRef.current = window.setTimeout(() => {
      hoverTimeoutRef.current = null;
      setPosition(null);
      setOpen(true);
    }, TOOLTIP_HOVER_DELAY_MS);
  };

  const handlePointerLeave = () => {
    clearHoverTimeout();
    setOpen(false);
  };

  const handleFocus = () => {
    clearHoverTimeout();
    setPosition(null);
    setOpen(true);
  };

  const handleBlur = () => {
    clearHoverTimeout();
    setOpen(false);
  };

  return (
    <span
      ref={anchorRef}
      className="inline-flex"
      data-command-id={commandId}
      tabIndex={disabledButton ? 0 : undefined}
      aria-disabled={disabledButton || undefined}
      aria-describedby={disabledButton ? describedBy : undefined}
      aria-keyshortcuts={
        disabledButton ? presentation.ariaKeyShortcuts || undefined : undefined
      }
      onPointerEnter={handlePointerEnter}
      onPointerLeave={handlePointerLeave}
      onFocusCapture={handleFocus}
      onBlurCapture={handleBlur}
    >
      {cloneElement(child, disabledButton ? {} : triggerProps)}
      {open &&
        createPortal(
          <span
            ref={tooltipRef}
            id={tooltipId}
            role="tooltip"
            className="app-tooltip command-tooltip pointer-events-none fixed z-[10000] max-w-[min(320px,calc(100vw-16px))] whitespace-nowrap overflow-hidden rounded-md border border-[var(--hairline-strong)] bg-[var(--surface-1)] px-2.5 py-1.5 text-[11px] leading-4 text-[var(--ink)] shadow-lg"
            style={{
              left: position?.left ?? 0,
              top: position?.top ?? 0,
              visibility: position ? 'visible' : 'hidden',
            }}
          >
            <span>{presentation.title}</span>
            {presentation.sequence.length > 0 && (
              <>
                {' '}
                <span className="ml-3 font-mono text-[10px] text-[var(--ink-tertiary)]">
                  {presentation.label}
                </span>
              </>
            )}
          </span>,
          document.body,
        )}
    </span>
  );
}
