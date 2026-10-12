import QtQuick
import Quickshell
import Quickshell.Io
import Quickshell.Wayland

// Headless service: starts the Gridfall screensaver (a native wgpu binary built from this repo) after the
// system idle timeout. Its window uses Omarchy's screensaver class, so the built-in idle service still locks on
// time and treats dismissing it as activity.
Item {
  id: root

  // injected by omarchy-shell's service loader: live access to shell.json (idle.screensaver)
  property var shell: null

  readonly property string pluginDir: Qt.resolvedUrl(".").toString().replace(/^file:\/\//, "").replace(/\/?$/, "/")
  readonly property var sysIdle: (shell && shell.shellConfig && shell.shellConfig.idle) ? shell.shellConfig.idle : null
  readonly property int idleSeconds: {
    var v = sysIdle ? Number(sysIdle.screensaver) : NaN
    return (isFinite(v) && v > 0) ? Math.floor(v) : 150
  }

  function log(m) { console.log("[gridfall] " + m) }

  function launch(reason) {
    if (launcher.running) return
    log("launch: " + reason)
    launcher.running = true
  }

  Process { id: launcher; command: ["bash", root.pluginDir + "bin/gridfall-launch"] }
  // the stock terminal screensaver would open alongside this one: it is switched off while the plugin runs
  // (and back on when the plugin is disabled, if this plugin was what switched it off)
  Process { id: builtinOff; command: ["bash", root.pluginDir + "bin/gridfall-builtin", "off"] }

  // IdleMonitor does not re-arm when its timeout changes, so a Loader builds a fresh one
  Loader {
    id: idleLoader
    active: false
    sourceComponent: Component {
      IdleMonitor {
        enabled: true
        timeout: root.idleSeconds
        respectInhibitors: true
        onIsIdleChanged: if (isIdle) root.launch("idle " + root.idleSeconds + "s")
      }
    }
  }
  Timer { id: armTimer; interval: 80; repeat: false; onTriggered: idleLoader.active = true }
  onIdleSecondsChanged: { idleLoader.active = false; armTimer.restart() }

  IpcHandler {
    target: "gridfall"
    function preview(): void { root.launch("preview") }
    function status(): string { return JSON.stringify({ idleSeconds: root.idleSeconds, armed: idleLoader.item !== null }) }
  }

  Component.onCompleted: { builtinOff.running = true; armTimer.restart() }
  // inline rather than via bin/gridfall-builtin: on removal the plugin folder may already be gone
  Component.onDestruction: Quickshell.execDetached(["bash", "-c",
    "m=\"${XDG_STATE_HOME:-$HOME/.local/state}/gridfall/disabled-builtin-screensaver\"; " +
    "if [ -f \"$m\" ]; then omarchy-toggle screensaver-off off; rm -f \"$m\"; fi"])
}
