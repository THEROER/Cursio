import { useToast, type ToastAction } from "@leavepulse/ui"

/** A failure stays long enough to read the node's line and decide what to do. */
const FAILURE_DURATION_MS = 9000

/** The message out of a `NodeError`, a thrown `Error`, or anything else. */
export function errorText(error: unknown): string {
  if (error && typeof error === "object" && "message" in error) {
    return String((error as { message: unknown }).message)
  }
  return String(error)
}

/**
 * The kit's toaster plus the failure shape the LeavePulse launcher settled on.
 *
 * What the node answers when something fails is a line written for a log ("no
 * previous version to go back to", "could not start …"): it names the cause
 * but not what failed. `failure` puts a plain title over it, and offers what is
 * worth having next to it — try again, and copy the line.
 */
export function useCursioToast() {
  const toast = useToast()
  const { t } = useI18n()

  function failure(error: unknown, options: { title?: string; retry?: () => unknown } = {}) {
    const detail = errorText(error)
    const actions: ToastAction[] = []
    if (options.retry) {
      const retry = options.retry
      actions.push({ label: t("toast.retry"), icon: "lucide:rotate-ccw", onClick: () => void retry() })
    }
    if (typeof navigator !== "undefined" && navigator.clipboard) {
      actions.push({
        label: t("toast.copy"),
        icon: "lucide:copy",
        onClick: async () => {
          await navigator.clipboard.writeText(detail)
          toast.success(t("toast.copied"))
        },
      })
    }
    return toast.error(detail, {
      title: options.title ?? t("toast.failed"),
      duration: FAILURE_DURATION_MS,
      actions,
    })
  }

  return { ...toast, failure }
}
