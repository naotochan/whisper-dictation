import { useEffect, useState } from "react";
import { useRecordingState } from "../hooks/useRecordingState";
import { getSettings } from "../lib/ipc";
import { translations, t, UILanguage } from "../lib/i18n";
import { applyAppearance, AppAppearance } from "../lib/theme";

const O = translations.overlay;

const demo = new URLSearchParams(window.location.search).get("demo");

/** Animated bars for the "processing" state. */
function ProcessingBars() {
  const bars = [
    { delay: "0s", height: "40%" },
    { delay: "0.15s", height: "70%" },
    { delay: "0.05s", height: "100%" },
    { delay: "0.2s", height: "60%" },
    { delay: "0.1s", height: "80%" },
  ];

  return (
    <div className="flex items-center gap-[3px]" style={{ height: "24px" }} aria-hidden="true">
      {bars.map((bar, i) => (
        <div
          key={i}
          className="rounded-full"
          style={{
            width: "4px",
            backgroundColor: "rgba(255,255,255,0.95)",
            height: bar.height,
            animation: "waveform 1s ease-in-out infinite",
            animationDelay: bar.delay,
          }}
        />
      ))}
    </div>
  );
}

/** Level-reactive bars for the "recording" state. */
function LevelBars({ level }: { level: number }) {
  const amplified = Math.min(1, Math.pow(level, 0.35) * 1.8);
  const offsets = [0.35, 0.75, 1.0, 0.6, 0.85];

  return (
    <div
      style={{ display: "flex", alignItems: "center", gap: "3px", height: "24px" }}
      aria-hidden="true"
    >
      {offsets.map((off, i) => {
        const barScale = 0.08 + amplified * off * 0.92;
        return (
          <div
            key={i}
            style={{
              width: "4px",
              borderRadius: "9999px",
              backgroundColor: "rgba(255,255,255,0.95)",
              height: "100%",
              transform: `scaleY(${barScale})`,
              transition: "transform 50ms ease-out",
            }}
          />
        );
      })}
    </div>
  );
}

function recordingGlow(level: number): string {
  const amplified = Math.min(1, Math.pow(level, 0.35) * 1.8);
  const blurTeal = 18 + amplified * 12;
  const alphaTeal = 0.35 + amplified * 0.25;
  const blurBlue = 50 + amplified * 20;
  const alphaBlue = 0.18 + amplified * 0.17;
  return `0 0 ${blurTeal}px rgba(45,212,191,${alphaTeal}), 0 0 ${blurBlue}px rgba(59,130,246,${alphaBlue})`;
}

function CheckIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#6ee7b7" strokeWidth="3" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <polyline points="20 6 9 17 4 12" />
    </svg>
  );
}

function WarningIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#fca5a5" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z" />
      <line x1="12" y1="9" x2="12" y2="13" />
      <line x1="12" y1="17" x2="12.01" y2="17" />
    </svg>
  );
}

export function OverlayIndicator() {
  const { state, audioLevel, lastTranscription, error } = useRecordingState();
  const [lang, setLang] = useState<UILanguage>("ja");
  const [demoLevel, setDemoLevel] = useState(0.5);

  useEffect(() => {
    getSettings()
      .then((s) => {
        setLang(s.ui_language || "ja");
        applyAppearance((s.appearance as AppAppearance) || "system");
      })
      .catch(() => {});
  }, []);

  useEffect(() => {
    if (demo !== "recording") return;
    const start = Date.now();
    const id = setInterval(() => {
      const t = (Date.now() - start) / 1000;
      const level = 0.35 + 0.3 * Math.sin(t * 2) + (Math.random() - 0.5) * 0.05;
      setDemoLevel(Math.min(1, Math.max(0, level)));
    }, 50);
    return () => clearInterval(id);
  }, []);

  const effectiveState =
    demo === "recording" ? "recording" : demo === "processing" ? "processing" : state;
  const effectiveLevel = demo === "recording" ? demoLevel : audioLevel;

  const isError = effectiveState === "error";
  const isRecording = effectiveState === "recording";
  const isProcessing = effectiveState === "processing";
  const isIdle = effectiveState === "idle";
  const showResult = isIdle && lastTranscription;

  const label = isProcessing
    ? t(O.processing, lang)
    : isRecording
      ? t(O.listening, lang)
      : "";

  const errorText =
    error?.trim() ||
    t(O.serverNotRunning, lang);

  const isGradientPill = isRecording || isProcessing;

  const pillStyle: React.CSSProperties = isGradientPill
    ? isProcessing
      ? {
          display: "inline-flex",
          alignItems: "center",
          gap: "10px",
          padding: "10px 18px",
          borderRadius: "9999px",
          background: "linear-gradient(120deg, #b45309, #f59e0b, #f97316, #f43f5e, #b45309)",
          backgroundSize: "300% 300%",
          animation: "gradientShift 6s ease-in-out infinite, glowPulse 2.4s ease-in-out infinite",
          maxWidth: "90vw",
        }
      : {
          display: "inline-flex",
          alignItems: "center",
          gap: "10px",
          padding: "10px 18px",
          borderRadius: "9999px",
          background: "linear-gradient(120deg, #0f766e, #06b6d4, #3b82f6, #8b5cf6, #0f766e)",
          backgroundSize: "300% 300%",
          animation: "gradientShift 6s ease-in-out infinite",
          boxShadow: recordingGlow(effectiveLevel),
          transition: "box-shadow 100ms ease-out",
          maxWidth: "90vw",
        }
    : {
        display: "inline-flex",
        alignItems: "center",
        gap: "10px",
        padding: "8px 16px",
        borderRadius: "9999px",
        backgroundColor: "rgba(0, 0, 0, 0.85)",
        boxShadow: "0 4px 12px rgba(0, 0, 0, 0.3)",
        maxWidth: "90vw",
      };

  return (
    <>
      <style>{`
        html, body, #root {
          margin: 0;
          padding: 0;
          width: 100%;
          height: 100%;
          background: transparent !important;
          background-color: transparent !important;
          overflow: hidden;
        }
        @keyframes waveform {
          0%, 100% { transform: scaleY(0.4); }
          50% { transform: scaleY(1); }
        }
        @keyframes gradientShift {
          0%, 100% { background-position: 0% 50%; }
          50% { background-position: 100% 50%; }
        }
        @keyframes glowPulse {
          0%, 100% { box-shadow: 0 0 18px rgba(251,191,36,0.35), 0 0 50px rgba(249,115,22,0.2); }
          50% { box-shadow: 0 0 30px rgba(251,191,36,0.55), 0 0 70px rgba(249,115,22,0.35); }
        }
        @media (prefers-reduced-motion: reduce) {
          * { animation: none !important; transition: none !important; }
        }
      `}</style>
      <div
        role="status"
        aria-live="polite"
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "center",
          width: "100%",
          height: "100%",
        }}
      >
        <div style={pillStyle}>
          {isError ? (
            <>
              <WarningIcon />
              <span
                style={{
                  fontSize: "11px",
                  fontWeight: 500,
                  color: "#fca5a5",
                  whiteSpace: "nowrap",
                  overflow: "hidden",
                  textOverflow: "ellipsis",
                  maxWidth: "400px",
                }}
              >
                {errorText}
              </span>
            </>
          ) : showResult ? (
            <>
              <CheckIcon />
              <span
                style={{
                  fontSize: "11px",
                  fontWeight: 500,
                  color: "rgba(255, 255, 255, 0.85)",
                  whiteSpace: "nowrap",
                  overflow: "hidden",
                  textOverflow: "ellipsis",
                  maxWidth: "400px",
                }}
              >
                {lastTranscription}
              </span>
            </>
          ) : (
            <>
              {isProcessing ? (
                <ProcessingBars />
              ) : (
                <LevelBars level={effectiveLevel} />
              )}
              <span
                style={{
                  fontSize: isGradientPill ? "12px" : "11px",
                  fontWeight: isGradientPill ? 600 : 500,
                  color: isGradientPill ? "#fff" : "rgba(255, 255, 255, 0.7)",
                  textShadow: isGradientPill ? "0 1px 6px rgba(0,0,0,0.35)" : undefined,
                  whiteSpace: "nowrap",
                }}
              >
                {label}
              </span>
            </>
          )}
        </div>
      </div>
    </>
  );
}
