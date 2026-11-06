"use client";

import { AnimatePresence, motion } from "motion/react";
import { type ReactNode, useCallback, useLayoutEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  LuArrowLeft,
  LuArrowRight,
  LuBriefcase,
  LuCookie,
  LuFolders,
  LuGithub,
  LuGlobe,
  LuHeart,
  LuLoaderCircle,
  LuNetwork,
  LuShieldCheck,
  LuTerminal,
  LuUsers,
} from "react-icons/lu";
import {
  IntroScene,
  LicenseScene,
  PermissionsScene,
} from "@/components/onboarding/scenes";
import { ProfileIsolationDemo } from "@/components/profile-isolation-demo";
import { SCENE_GRID_CLASS } from "@/components/tips/scene-primitives";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { StepDots } from "@/components/ui/step-dots";
import { StepTransition } from "@/components/ui/step-transition";
import { useMotionAllowed } from "@/hooks/use-motion-allowed";
import { usePermissions } from "@/hooks/use-permissions";
import { MOTION_DURATION, MOTION_EASE_IN, MOTION_SPRING } from "@/lib/motion";
import { getCurrentOS } from "@/lib/platform";
import { cn } from "@/lib/utils";

type WelcomeStep = "intro" | "isolation" | "license" | "permissions";

// Concrete features shown on the intro step.
const FEATURES = [
  { key: "welcome.features.items.profiles", Icon: LuUsers },
  { key: "welcome.features.items.proxy", Icon: LuNetwork },
  { key: "welcome.features.items.vpn", Icon: LuShieldCheck },
  { key: "welcome.features.items.cookies", Icon: LuCookie },
  { key: "welcome.features.items.groups", Icon: LuFolders },
  { key: "welcome.features.items.api", Icon: LuTerminal },
  { key: "welcome.features.items.setDefault", Icon: LuGlobe },
  { key: "welcome.features.items.openSource", Icon: LuGithub },
] as const;

function StepHeading({
  title,
  body,
  tone,
}: {
  title: ReactNode;
  body: ReactNode;
  tone?: "error";
}) {
  return (
    <div className="flex flex-col items-center gap-2 text-center">
      <h2
        className={cn(
          "flex items-center justify-center gap-2 text-2xl font-semibold tracking-tight text-balance",
          tone === "error" && "text-destructive-text",
        )}
      >
        {title}
      </h2>
      <p className="mx-auto max-w-[52ch] text-base/7 text-pretty text-muted-foreground sm:text-sm/6">
        {body}
      </p>
    </div>
  );
}

/**
 * The first-run welcome: a few short steps, each with its own drawing. The
 * drawing slides in the direction of travel and the step's text moves a few
 * pixels with it; the dots in the footer show where the person is.
 *
 * Steps travel, they do not fade in. Onboarding is the first thing a new
 * install shows and the only way past it is the button on the current step,
 * so a step that fails to appear is a dead app. Text and controls are at full
 * opacity from the first frame and only move into place, so a stalled frame
 * clock costs a few pixels of offset, never the screen. Only the decorative
 * drawing fades.
 */
export function WelcomeDialog({
  isOpen,
  onComplete,
}: {
  isOpen: boolean;
  onComplete: () => void;
}) {
  const { t } = useTranslation();
  const animateSteps = useMotionAllowed();
  const {
    requestPermission,
    isMicrophoneAccessGranted,
    isCameraAccessGranted,
    isInitialized,
    requiresSystemPermissions,
  } = usePermissions(isOpen);
  const [step, setStep] = useState<WelcomeStep>("intro");
  const [direction, setDirection] = useState<1 | -1>(1);
  const permissionsGranted = isMicrophoneAccessGranted && isCameraAccessGranted;
  const showPermissionsStep =
    requiresSystemPermissions &&
    (step === "permissions" || !isInitialized || !permissionsGranted);
  const visibleSteps: WelcomeStep[] = [
    "intro",
    "isolation",
    "license",
    ...(showPermissionsStep ? (["permissions"] as const) : []),
  ];
  const currentStepIndex = Math.max(0, visibleSteps.indexOf(step));
  const go = (next: WelcomeStep) => {
    setDirection(visibleSteps.indexOf(next) >= currentStepIndex ? 1 : -1);
    setStep(next);
  };
  const back = () => {
    const previous = visibleSteps[currentStepIndex - 1];
    if (previous) go(previous);
  };
  const [requesting, setRequesting] = useState(false);

  // Each step starts at its top, also when the one before was scrolled.
  // biome-ignore lint/correctness/useExhaustiveDependencies: the step is the trigger
  useLayoutEffect(() => {
    document
      .querySelector('[data-slot="welcome-dialog"]')
      ?.scrollTo({ top: 0 });
  }, [step]);

  const requestPermissions = useCallback(async () => {
    setRequesting(true);
    try {
      if (!isMicrophoneAccessGranted) {
        await requestPermission("microphone");
      }
      if (!isCameraAccessGranted) {
        await requestPermission("camera");
      }
    } catch (err) {
      console.error("Permission request failed:", err);
    } finally {
      setRequesting(false);
      onComplete();
    }
  }, [
    isCameraAccessGranted,
    isMicrophoneAccessGranted,
    onComplete,
    requestPermission,
  ]);

  const acceptLicense = () => {
    if (getCurrentOS() === "macos" && !(isInitialized && permissionsGranted)) {
      go("permissions");
    } else {
      onComplete();
    }
  };

  const scene =
    step === "intro" ? (
      <IntroScene />
    ) : step === "license" ? (
      <LicenseScene />
    ) : step === "permissions" ? (
      <PermissionsScene />
    ) : null;

  const skipButton = (
    <Button
      variant="ghost"
      size="sm"
      className="text-muted-foreground hover:text-foreground"
      onClick={onComplete}
    >
      {t("welcome.skip")}
    </Button>
  );
  const backButton = (
    <Button
      type="button"
      variant="subtle"
      size="icon"
      className="size-8"
      aria-label={t("common.buttons.back")}
      data-slot="welcome-back"
      onClick={back}
    >
      <LuArrowLeft className="size-4" />
    </Button>
  );

  let body: ReactNode = null;
  let leading: ReactNode = null;
  let trailing: ReactNode = null;

  switch (step) {
    case "intro":
      body = (
        <>
          <StepHeading title={t("welcome.title")} body={t("welcome.tagline")} />
          <ul
            aria-label={t("welcome.features.title")}
            data-slot="welcome-features"
            className="flex flex-wrap justify-center gap-1.5"
          >
            {FEATURES.map(({ key, Icon }, index) => (
              <motion.li
                key={key}
                initial={animateSteps ? { y: 6, scale: 0.97 } : false}
                animate={{ y: 0, scale: 1 }}
                transition={
                  animateSteps
                    ? {
                        type: "spring",
                        stiffness: 420,
                        damping: 28,
                        delay: 0.12 + index * 0.035,
                      }
                    : { duration: 0 }
                }
                className="flex items-center gap-1.5 rounded-full bg-foreground/5 px-2.5 py-1 text-xs text-foreground"
              >
                <Icon
                  className="size-3.5 shrink-0 text-muted-foreground"
                  aria-hidden="true"
                />
                {t(key)}
              </motion.li>
            ))}
          </ul>
        </>
      );
      leading = skipButton;
      trailing = (
        <Button size="sm" className="gap-1.5" onClick={() => go("isolation")}>
          {t("welcome.next")}
          <LuArrowRight className="size-4 shrink-0" />
        </Button>
      );
      break;

    case "isolation":
      body = (
        <>
          <StepHeading
            title={t("isolationDemo.title")}
            body={t("isolationDemo.description")}
          />
          <ProfileIsolationDemo showHeading={false} />
        </>
      );
      leading = skipButton;
      trailing = (
        <>
          {backButton}
          <Button size="sm" className="gap-1.5" onClick={() => go("license")}>
            {t("welcome.next")}
            <LuArrowRight className="size-4 shrink-0" />
          </Button>
        </>
      );
      break;

    case "license":
      body = (
        <>
          <StepHeading
            title={t("welcome.license.title")}
            body={t("welcome.license.body")}
          />
          <dl className="flex flex-col gap-2">
            <div className="flex items-center gap-3 rounded-xl bg-foreground/[0.035] px-4 py-2.5 ring-1 ring-foreground/6 ring-inset">
              <LuHeart className="size-4 shrink-0 text-success-text" />
              <div className="flex min-w-0 flex-wrap items-baseline gap-x-2 text-left">
                <dt className="text-base/7 font-medium text-foreground sm:text-sm/6">
                  {t("welcome.license.personalTitle")}
                </dt>
                <dd className="text-base/7 text-pretty text-muted-foreground sm:text-sm/6">
                  {t("welcome.license.personalDesc")}
                </dd>
              </div>
            </div>
            <div className="flex items-start gap-3 rounded-xl bg-foreground/[0.035] px-4 py-2.5 ring-1 ring-foreground/6 ring-inset">
              <LuBriefcase className="mt-1 size-4 shrink-0 text-muted-foreground" />
              <div className="flex min-w-0 flex-col text-left">
                <dt className="flex flex-wrap items-center gap-2 text-base/7 font-medium text-foreground sm:text-sm/6">
                  {t("welcome.license.commercialTitle")}
                  <span className="rounded-full bg-primary/10 px-2 py-0.5 text-xs font-medium text-primary-text">
                    {t("welcome.license.trialBadge")}
                  </span>
                </dt>
                <dd className="text-base/7 text-pretty text-muted-foreground sm:text-sm/6">
                  {t("welcome.license.commercialDesc")}
                </dd>
              </div>
            </div>
          </dl>
        </>
      );
      leading = skipButton;
      trailing = (
        <>
          {backButton}
          <Button size="sm" className="gap-1.5" onClick={acceptLicense}>
            {t("welcome.license.agree")}
            <LuArrowRight className="size-4 shrink-0" />
          </Button>
        </>
      );
      break;

    case "permissions":
      body = (
        <StepHeading
          title={t("welcome.permissions.title")}
          body={t("welcome.permissions.desc")}
        />
      );
      leading = (
        <Button
          variant="ghost"
          size="sm"
          className="text-muted-foreground hover:text-foreground"
          disabled={requesting}
          onClick={onComplete}
        >
          {t("welcome.permissions.skip")}
        </Button>
      );
      trailing = (
        <Button
          size="sm"
          className="gap-1.5"
          disabled={requesting}
          onClick={() => {
            void requestPermissions();
          }}
        >
          {requesting && (
            <LuLoaderCircle className="size-4 shrink-0 motion-safe:animate-spin" />
          )}
          {requesting
            ? t("welcome.permissions.requesting")
            : t("welcome.permissions.grant")}
        </Button>
      );
      break;
  }

  return (
    <Dialog open={isOpen} onOpenChange={() => {}}>
      <DialogContent
        dismissible={false}
        data-slot="welcome-dialog"
        data-step={step}
        className="gap-0 overflow-x-hidden p-0 sm:max-w-xl"
      >
        <DialogTitle className="sr-only">{t("welcome.title")}</DialogTitle>

        {scene && (
          <div className="px-3 pt-3 sm:px-4 sm:pt-4 [@media(max-height:440px)]:hidden">
            <div
              data-slot="welcome-scene-panel"
              className={cn(
                "relative aspect-[3/1] w-full overflow-hidden rounded-xl bg-foreground/[0.025] ring-1 ring-foreground/6 ring-inset [@media(min-height:541px)_and_(max-height:640px)]:aspect-[4/1] [@media(max-height:540px)]:aspect-[5/1]",
                SCENE_GRID_CLASS,
              )}
            >
              <AnimatePresence mode="popLayout" initial={false}>
                <motion.div
                  key={step}
                  className="absolute inset-0 p-3"
                  // The new drawing is half visible on its first frame; the
                  // old one leaves quickly and never holds the new one back.
                  initial={
                    animateSteps
                      ? { x: direction * 40, opacity: 0.5, scale: 0.98 }
                      : false
                  }
                  animate={{ x: 0, opacity: 1, scale: 1 }}
                  exit={
                    animateSteps
                      ? {
                          x: -direction * 40,
                          opacity: 0,
                          scale: 0.98,
                          transition: {
                            duration: MOTION_DURATION.fast,
                            ease: MOTION_EASE_IN,
                          },
                        }
                      : { opacity: 0, transition: { duration: 0 } }
                  }
                  transition={
                    animateSteps ? MOTION_SPRING.gentle : { duration: 0 }
                  }
                >
                  {scene}
                </motion.div>
              </AnimatePresence>
            </div>
          </div>
        )}

        <StepTransition
          transitionKey={step}
          direction={direction}
          className="flex min-h-36 flex-col gap-5 px-5 pt-5 sm:px-8 [@media(max-height:640px)]:gap-4 [@media(max-height:640px)]:pt-4"
        >
          {body}
        </StepTransition>

        <div className="sticky bottom-0 z-10 grid grid-cols-[1fr_auto_1fr] items-center gap-3 bg-background px-4 pt-5 pb-4 sm:px-5 sm:pb-5 [@media(max-height:640px)]:pt-4">
          <div className="flex min-w-0 items-center justify-start">
            {leading}
          </div>
          <StepDots
            count={visibleSteps.length}
            current={currentStepIndex}
            label={t("welcome.progress")}
          />
          <div className="flex min-w-0 items-center justify-end gap-1.5">
            {trailing}
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}
