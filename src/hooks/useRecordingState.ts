import { useState, useEffect } from "react";
import { listen } from "@tauri-apps/api/event";

export type RecordingState = "idle" | "recording" | "processing" | "error";

interface RecordingStatePayload {
  state: RecordingState;
}

interface TranscriptionResultPayload {
  text: string;
  raw_text: string;
  language: string;
}

interface ErrorPayload {
  message: string;
}

interface AudioLevelPayload {
  level: number;
}

export function useRecordingState() {
  const [state, setState] = useState<RecordingState>("idle");
  const [lastTranscription, setLastTranscription] = useState<string>("");
  const [lastRawTranscription, setLastRawTranscription] = useState<string>("");
  const [error, setError] = useState<string>("");
  const [audioLevel, setAudioLevel] = useState<number>(0);

  useEffect(() => {
    const unlisteners: (() => void)[] = [];

    listen<RecordingStatePayload>("recording-state", (event) => {
      setState(event.payload.state);
      if (event.payload.state === "idle") {
        setError("");
      }
      if (event.payload.state !== "recording") {
        setAudioLevel(0);
      }
    })
      .then((unlisten) => unlisteners.push(unlisten))
      .catch(() => {});

    listen<TranscriptionResultPayload>("transcription-result", (event) => {
      setLastTranscription(event.payload.text);
      setLastRawTranscription(event.payload.raw_text);
    })
      .then((unlisten) => unlisteners.push(unlisten))
      .catch(() => {});

    listen<ErrorPayload>("error", (event) => {
      setError(event.payload.message);
    })
      .then((unlisten) => unlisteners.push(unlisten))
      .catch(() => {});

    listen<AudioLevelPayload>("audio-level", (event) => {
      setAudioLevel(event.payload.level);
    })
      .then((unlisten) => unlisteners.push(unlisten))
      .catch(() => {});

    return () => {
      unlisteners.forEach((fn) => fn());
    };
  }, []);

  const clearResults = () => {
    setLastTranscription("");
    setLastRawTranscription("");
    setError("");
  };

  return { state, lastTranscription, lastRawTranscription, error, audioLevel, clearResults };
}
