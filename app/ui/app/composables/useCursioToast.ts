import { getVersion } from "@tauri-apps/api/app"
import { useToast, type ToastAction } from "@leavepulse/ui"
import { localNodeName, type NodeError } from "~/composables/useNode"

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
 * Everything worth pasting into a bug report: what was being done (the title,
 * the command and its arguments — which peer, which module, which value),
 * where (this machine, the page, the app's version), when, and the node's own
 * line. The line alone ("the node did not answer within 5 s") says nothing of
 * which of a dozen requests it was about.
 */
async function report(title: string, error: unknown, at: Date): Promise<string> {
  const failure = (error && typeof error === "object" ? error : {}) as Partial<NodeError>
  let version = "unknown"
  try {
    version = await getVersion()
  } catch {
    // Outside the Tauri shell (a browser preview) there is no app version.
  }
  const lines = [
    `Cursio: ${title}`,
    `error: ${errorText(error)}`,
    failure.kind ? `kind: ${failure.kind}` : null,
    failure.command ? `command: ${failure.command}` : null,
    failure.args && Object.keys(failure.args).length ? `args: ${JSON.stringify(failure.args)}` : null,
    `machine: ${localNodeName() ?? "unknown (the node has not reported)"}`,
    typeof location !== "undefined" ? `page: ${location.pathname}${location.hash}` : null,
    `app: ${version}`,
    `at: ${at.toISOString()} (local ${at.toLocaleString()})`,
  ]
  return lines.filter((line): line is string => line !== null).join("\n")
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
    const title = options.title ?? t("toast.failed")
    const at = new Date()
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
          await navigator.clipboard.writeText(await report(title, error, at))
          toast.success(t("toast.copied"))
        },
      })
    }
    return toast.error(detail, {
      title,
      duration: FAILURE_DURATION_MS,
      actions,
    })
  }

  return { ...toast, failure }
}
