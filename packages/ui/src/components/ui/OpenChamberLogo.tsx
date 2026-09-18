import React, { useMemo } from 'react';
import { useOptionalThemeSystem } from '@/contexts/useThemeSystem';
import { useI18n } from '@/lib/i18n';

interface OMPChamberLogoProps {
  className?: string;
  width?: number;
  height?: number;
  isAnimated?: boolean;
}

// The official Oh My Pi mark (https://github.com/can1357/oh-my-pi/blob/main/assets/icon.svg):
// a π built from a top bar and two legs, with an orange plugin connector (two slots cut
// out via even-odd) resting on the right leg, plus two accent dots on the bar. Coordinates
// are the SVG source units (content spans x:10…110, y:8…82 in a 120×90 system), uniformly
// scaled and centered into the 100×100 view box. The π bars follow the themed surface
// foreground; the connector keeps the official orange on every theme.
export const OMPChamberLogo: React.FC<OMPChamberLogoProps> = ({
  className = '',
  width = 70,
  height = 70,
  isAnimated = false,
}) => {
  const { t } = useI18n();
  const themeContext = useOptionalThemeSystem();

  let isDark = true;
  if (themeContext) {
    isDark = themeContext.currentTheme.metadata.variant !== 'light';
  } else if (typeof window !== 'undefined') {
    isDark = window.matchMedia('(prefers-color-scheme: dark)').matches;
  }

  const strokeColor = useMemo(() => {
    if (themeContext) {
      return themeContext.currentTheme.colors.surface.foreground;
    }
    if (typeof window !== 'undefined') {
      const fromVars = getComputedStyle(document.documentElement).getPropertyValue('--splash-stroke').trim();
      if (fromVars) {
        return fromVars;
      }
    }
    return isDark ? 'white' : 'black';
  }, [themeContext, isDark]);

  return (
    <svg
      width={width}
      height={height}
      viewBox="0 0 100 100"
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
      className={className}
      role="img"
      aria-label={t('openChamberLogo.aria.logo')}
    >
      {isAnimated ? (
        <style>{`@keyframes oc-logo-glow{0%,100%{filter:drop-shadow(0 0 0 transparent)}50%{filter:drop-shadow(0 0 4px var(--oc-glow-color))}}.oc-logo-glow{animation:oc-logo-glow 1.8s ease-in-out infinite}@media (prefers-reduced-motion:reduce){.oc-logo-glow{animation:none}}`}</style>
      ) : null}
      <g transform="translate(-2.8 10.4) scale(0.88)">
        {/* π: top bar + two legs */}
        <rect x="10" y="8" width="100" height="12" rx="2" fill={strokeColor} />
        <rect x="25" y="20" width="12" height="62" rx="2" fill={strokeColor} />
        <rect x="75" y="20" width="12" height="45" rx="2" fill={strokeColor} />
        {/* Orange plugin connector with two slots cut out (even-odd), plus accent dots */}
        <g
          className={isAnimated ? 'oc-logo-glow' : undefined}
          style={isAnimated ? ({ '--oc-glow-color': '#f97316' } as React.CSSProperties) : undefined}
        >
          <path
            fillRule="evenodd"
            clipRule="evenodd"
            d="M74 55 H88 A3 3 0 0 1 91 58 V68 A3 3 0 0 1 88 71 H74 A3 3 0 0 1 71 68 V58 A3 3 0 0 1 74 55 Z M76 59 h3 v8 h-3 Z M82 59 h3 v8 h-3 Z"
            fill="#f97316"
          />
          <circle cx="18" cy="14" r="2" fill="#f97316" opacity="0.8" />
          <circle cx="102" cy="14" r="2" fill="#f97316" opacity="0.8" />
        </g>
      </g>
    </svg>
  );
};
