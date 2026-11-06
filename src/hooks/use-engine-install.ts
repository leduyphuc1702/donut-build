import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useEffect, useState } from "react";
import i18n from "@/i18n";
import { translateBackendError } from "@/lib/backend-errors";
import { getBrowserDisplayName } from "@/lib/browser-utils";
import {
  dismissToast,
  showDownloadToast,
  showErrorToast,
  showToast,
} from "@/lib/toast-utils";

const ENGINE = "apostate";

interface EngineProgress {
  browser: string;
  version: string;
  percentage: number;
  speed_bytes_per_sec: number;
  eta_seconds?: number;
  stage: string;
  error?: string;
}

interface GeoipProgress {
  stage: string;
  percentage: number;
  speed_bytes_per_sec?: number;
  eta_seconds?: number;
}

export interface EngineInstallStatus {
  installed: boolean;
  supported: boolean;
  /** The engine's own release, and the Chromium it is built on. */
  version: string;
  chromium: string;
}

function formatTime(seconds: number): string {
  if (seconds < 60) return `${Math.round(seconds)}s`;
  if (seconds < 3600) {
    return `${Math.floor(seconds / 60)}m ${Math.round(seconds % 60)}s`;
  }
  return `${Math.floor(seconds / 3600)}h ${Math.floor((seconds % 3600) / 60)}m`;
}

function progressDetail(speed?: number, eta?: number) {
  return {
    ...(speed ? { speed: (speed / (1024 * 1024)).toFixed(1) } : {}),
    ...(eta ? { eta: formatTime(eta) } : {}),
  };
}

/**
 * Shows the engine's first download, and the GeoIP database's, as toasts. The
 * app does not wait for either: the backend starts them at startup and a launch
 * that finds the engine missing waits for the same download.
 */
export function useEngineInstallToasts() {
  useEffect(() => {
    let disposed = false;
    const stops: (() => void)[] = [];

    const retry = () => {
      void invoke("download_browser", {
        browserStr: "wayfern",
        version: "",
      }).catch((error: unknown) => {
        console.error("Engine download retry failed:", error);
      });
    };

    void (async () => {
      try {
        const stopEngine = await listen<EngineProgress>(
          "download-progress",
          (event) => {
            const progress = event.payload;
            if (progress.browser !== ENGINE) return;
            const name = getBrowserDisplayName(progress.browser);
            const toastId = `download-${name.toLowerCase()}-${progress.version}`;
            switch (progress.stage) {
              case "downloading":
                showDownloadToast(name, progress.version, "downloading", {
                  percentage: progress.percentage,
                  ...progressDetail(
                    progress.speed_bytes_per_sec,
                    progress.eta_seconds,
                  ),
                  ...(progress.eta_seconds
                    ? {}
                    : { eta: i18n.t("browserDownload.toast.calculating") }),
                });
                break;
              case "extracting":
                showDownloadToast(
                  name,
                  progress.version,
                  "extracting",
                  progress.percentage > 0
                    ? { percentage: progress.percentage }
                    : undefined,
                );
                break;
              case "verifying":
                showDownloadToast(name, progress.version, "verifying");
                break;
              case "completed":
                showDownloadToast(name, progress.version, "completed");
                break;
              case "error":
                dismissToast(toastId);
                showErrorToast(
                  i18n.t("browserDownload.toast.downloadFailed", {
                    browser: name,
                    version: progress.version,
                  }),
                  {
                    id: "engine-download-error",
                    description: progress.error
                      ? translateBackendError(i18n.t, progress.error)
                      : i18n.t(
                          "browserDownload.toast.downloadFailedDescription",
                        ),
                    action: {
                      label: i18n.t("common.buttons.retry"),
                      onClick: retry,
                    },
                  },
                );
                break;
            }
          },
        );
        stops.push(stopEngine);

        const stopGeoip = await listen<GeoipProgress>(
          "geoip-download-progress",
          (event) => {
            const { stage, percentage, speed_bytes_per_sec, eta_seconds } =
              event.payload;
            if (stage === "downloading") {
              showToast({
                id: "geoip-download",
                type: "download",
                title: i18n.t("browserDownload.toast.geoipDownloading"),
                stage: "downloading",
                progress: {
                  percentage,
                  ...progressDetail(speed_bytes_per_sec, eta_seconds),
                },
              });
            } else if (stage === "completed") {
              showToast({
                id: "geoip-download",
                type: "download",
                title: i18n.t("browserDownload.toast.geoipDownloaded"),
                stage: "completed",
              });
            }
          },
        );
        stops.push(stopGeoip);
      } catch (error) {
        console.error("Failed to listen for download progress:", error);
      }
      if (disposed) for (const stop of stops) stop();
    })();

    return () => {
      disposed = true;
      for (const stop of stops) stop();
    };
  }, []);
}

/**
 * The engine's install state: null until the backend has answered. Not
 * installed turns installed when the download completes, so a dialog that says
 * "the engine is still being prepared" clears by itself.
 */
export function useEngineInstall(enabled = true): EngineInstallStatus | null {
  const [status, setStatus] = useState<EngineInstallStatus | null>(null);

  useEffect(() => {
    if (!enabled) return;
    let disposed = false;
    let stop: (() => void) | undefined;

    void invoke<EngineInstallStatus>("get_engine_install_status")
      .then((next) => {
        if (!disposed) setStatus(next);
      })
      .catch((error: unknown) => {
        console.error("Failed to read the engine status:", error);
      });
    void listen<EngineProgress>("download-progress", (event) => {
      if (event.payload.browser !== ENGINE) return;
      if (event.payload.stage === "completed") {
        setStatus((previous) =>
          previous ? { ...previous, installed: true } : previous,
        );
      }
    }).then((unlisten) => {
      if (disposed) unlisten();
      else stop = unlisten;
    });

    return () => {
      disposed = true;
      stop?.();
    };
  }, [enabled]);

  return status;
}

/** `useEngineInstall` for callers that only ask whether the engine is on disk. */
export function useEngineInstalled(enabled = true): boolean | null {
  return useEngineInstall(enabled)?.installed ?? null;
}
