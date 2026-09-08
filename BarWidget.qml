import QtQuick
import Quickshell
import Quickshell.Io
import qs.Ui
import qs.Commons

// Status + control for the mwb-omarchy-bridge daemon
// (~/Work/mwb-omarchy-bridge, systemd --user service
// mwb-omarchy-bridge.service — see that repo's PROTOCOL.md for the full
// story). The daemon reads its config once at startup from
// ~/.local/share/omarchy-mwb-bridge/config.json (no live-reload — this
// widget restarts the service after saving a change) and reports live
// connection state to .../status.json, which this widget polls for
// display. Service run-state is polled separately via `systemctl --user
// is-active`, since a crashed or manually-stopped daemon simply stops
// updating status.json rather than reporting itself as stopped in it.
BarWidget {
  id: root
  moduleName: "io.github.alanone.mwb-bridge"
  // The bar's ModuleSlot sizes itself from this root item's own
  // implicitWidth/Height, which a bare Item never derives from an
  // anchors.fill child on its own — without this the whole widget collapses
  // to a zero-width slot and never actually appears (verified empirically:
  // even a minimal BarIconButton-only version was invisible until this was
  // added).
  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  readonly property real popupWidth: Math.max(200, Number(root.setting("popupWidth", 340)) || 340)
  readonly property string home: Quickshell.env("HOME")
  readonly property string dataDir: root.home + "/.local/share/omarchy-mwb-bridge"
  readonly property string configPath: root.dataDir + "/config.json"
  readonly property string statusPath: root.dataDir + "/status.json"
  readonly property string serviceName: "mwb-omarchy-bridge.service"

  // This widget's own directory, whatever it was actually installed at —
  // resolved from the QML file's own URL rather than assuming the
  // canonical ~/.config/omarchy/plugins/... path, so first-run setup below
  // works regardless of where this got checked out.
  readonly property string pluginDir: {
    var url = Qt.resolvedUrl(".").toString()
    if (url.indexOf("file://") === 0) url = url.substring("file://".length)
    return url.replace(/\/+$/, "")
  }
  readonly property string daemonDir: root.pluginDir + "/daemon"
  // Built outside the plugin directory entirely: cargo writes thousands of
  // files under target/ during a build, and Quickshell's local-plugin
  // file-watcher reacts to changes anywhere under the plugin's own
  // directory tree — building in-place was observed triggering repeated
  // mid-build widget reloads (each harmlessly re-checking already-done
  // setup steps, but wasteful, and a plausible source of races with an
  // in-flight setctl chain). A fixed cache dir sidesteps that entirely.
  readonly property string buildCacheDir: root.home + "/.cache/omarchy-mwb-bridge-build"
  readonly property string daemonBinaryPath: root.buildCacheDir + "/release/daemon"
  readonly property string systemdUnitPath: root.home + "/.config/systemd/user/" + root.serviceName

  // Non-empty only while first-run setup (build + install the systemd
  // unit) is actually in progress or has just failed; shown in the popup.
  property string setupStatus: ""

  property bool popupOpen: false
  property bool showKey: false
  function close() { root.popupOpen = false }

  // Live daemon-reported connection state (status.json).
  property bool statusConnected: false
  property string statusPeer: ""
  property string statusDetail: "Loading..."

  // systemd unit state — separate from the above, see file-level comment.
  property string serviceState: "unknown"

  readonly property bool connected: root.statusConnected && root.serviceState === "active"
  readonly property string tooltipText: root.serviceState !== "active"
    ? "MWB Bridge: service " + root.serviceState
    : (root.statusConnected ? "Connected to " + root.statusPeer : root.statusDetail)

  // Never left at 0: seeded eagerly in Component.onCompleted so a fresh
  // install (no config.json yet) still has a valid id to show/save before
  // the FileView's async load-failed callback would otherwise supply one.
  property int machineId: 0
  property string saveMessage: ""

  // This machine's actual active XKB layout/variant — read fresh from
  // Hyprland (never hardcoded) whenever settings are saved, so the daemon's
  // virtual keyboard always matches whatever this specific install is
  // actually running, not whatever machine the plugin was written on.
  property string detectedXkbLayout: "us"
  property string detectedXkbVariant: ""

  function refreshServiceState() { serviceStateProc.running = true }
  function detectXkbLayout() {
    xkbLayoutProc.running = true
    xkbVariantProc.running = true
  }

  Component.onCompleted: {
    if (root.machineId === 0) root.machineId = Math.floor(Math.random() * 4294967295)
    root.detectXkbLayout()
    root.ensureDaemonSetUp()
  }

  // First-run setup: build the daemon binary if it's missing, then make
  // sure the systemd unit exists and is enabled — both are no-ops (just
  // the two cheap `test` checks) on every load after the first, so this
  // never fights a user's own Start/Stop choice on subsequent widget loads.
  function ensureDaemonSetUp() { checkBinaryProc.running = true }

  Process {
    id: checkBinaryProc
    command: ["test", "-x", root.daemonBinaryPath]
    onExited: function(exitCode) {
      if (exitCode === 0) {
        root.ensureServiceUnit()
      } else {
        root.setupStatus = "First-time setup: building the daemon (can take a minute)..."
        buildDaemonProc.running = true
      }
    }
  }

  Process {
    id: buildDaemonProc
    command: ["cargo", "build", "--release", "--target-dir", root.buildCacheDir]
    workingDirectory: root.daemonDir
    onExited: function(exitCode) {
      if (exitCode === 0) {
        root.setupStatus = ""
        root.ensureServiceUnit()
      } else {
        root.setupStatus = "Daemon build failed — install Rust (rustup.rs), then run `cargo build --release` in " + root.daemonDir + " yourself."
      }
    }
  }

  function ensureServiceUnit() { checkUnitProc.running = true }

  Process {
    id: checkUnitProc
    command: ["test", "-f", root.systemdUnitPath]
    onExited: function(exitCode) {
      if (exitCode === 0) {
        root.refreshServiceState()
      } else {
        mkdirSystemdDirProc.running = true
      }
    }
  }

  Process {
    id: mkdirSystemdDirProc
    command: ["mkdir", "-p", root.home + "/.config/systemd/user"]
    onExited: function(exitCode) { root.writeServiceUnit() }
  }

  function writeServiceUnit() {
    var unit =
      "[Unit]\n" +
      "Description=MWB Omarchy Bridge (Mouse Without Borders client/server daemon)\n" +
      "After=graphical-session.target\n" +
      "PartOf=graphical-session.target\n" +
      "\n" +
      "[Service]\n" +
      "ExecStart=" + root.daemonBinaryPath + "\n" +
      "Restart=always\n" +
      "RestartSec=3\n" +
      "\n" +
      "[Install]\n" +
      "WantedBy=graphical-session.target\n"
    unitFile.setText(unit)
    daemonReloadProc.running = true
  }

  FileView {
    id: unitFile
    path: root.systemdUnitPath
    printErrors: false
    atomicWrites: true
  }

  Process {
    id: daemonReloadProc
    command: ["systemctl", "--user", "daemon-reload"]
    onExited: function(exitCode) { enableServiceProc.running = true }
  }

  Process {
    id: enableServiceProc
    command: ["systemctl", "--user", "enable", "--now", root.serviceName]
    onExited: function(exitCode) {
      // "enable --now" occasionally leaves the unit enabled-but-not-started
      // (seen once during testing, cause unclear — possibly a race with
      // graphical-session.target during a shell restart) — one explicit
      // start covers that case for free; a no-op if it's already running.
      startAfterInstallProc.running = true
    }
  }

  Process {
    id: startAfterInstallProc
    command: ["systemctl", "--user", "start", root.serviceName]
    onExited: function(exitCode) { root.refreshServiceState() }
  }

  FileView {
    id: statusFile
    path: root.statusPath
    watchChanges: true
    printErrors: false
    onFileChanged: reload()
    onLoaded: {
      try {
        var s = JSON.parse(String(text() || ""))
        root.statusConnected = !!s.connected
        root.statusPeer = String(s.peer || "")
        root.statusDetail = String(s.detail || "")
      } catch (e) {
        // Malformed/mid-write — keep the last-known state, next change
        // event (or the daemon's next status write) will fix it.
      }
    }
    onLoadFailed: function(error) {
      root.statusConnected = false
      root.statusDetail = "No status yet — is the service running?"
    }
  }

  FileView {
    id: configFile
    path: root.configPath
    watchChanges: false
    atomicWrites: true
    printErrors: false
    onLoaded: {
      try {
        var c = JSON.parse(String(text() || ""))
        keyField.text = String(c.security_key || "")
        ipField.text = String(c.windows_ip || "")
        nameField.text = String(c.machine_name || "")
        var id = Number(c.machine_id)
        if (id) root.machineId = id
        // Fallback only — detectXkbLayout() (already running from
        // Component.onCompleted) overwrites these with a fresh live read
        // moments later. Keeping whatever was last saved here means a
        // failed hyprctl call can't silently reset the layout to "us".
        if (c.xkb_layout) root.detectedXkbLayout = String(c.xkb_layout)
        if (c.xkb_variant !== undefined) root.detectedXkbVariant = String(c.xkb_variant)
      } catch (e) {
        // Malformed — leave the form as-is (either still empty, on first
        // ever load, or whatever the user was already editing).
      }
    }
  }

  function saveSettings() {
    var cfg = {
      security_key: keyField.text,
      windows_ip: ipField.text.trim(),
      machine_name: nameField.text.trim(),
      machine_id: root.machineId,
      xkb_layout: root.detectedXkbLayout,
      xkb_variant: root.detectedXkbVariant
    }
    configFile.setText(JSON.stringify(cfg, null, 2) + "\n")
    chmodProc.running = true
    restartProc.running = true
    root.saveMessage = "Saved — restarting service..."
  }

  // `hyprctl getoption` is the live source of truth for this machine's
  // actual active layout — never hardcode a layout here, it has to match
  // whatever install this actually is.
  Process {
    id: xkbLayoutProc
    command: ["hyprctl", "getoption", "input:kb_layout", "-j"]
    stdout: StdioCollector {
      waitForEnd: true
      onStreamFinished: {
        try {
          var parsed = JSON.parse(String(text || ""))
          if (parsed.str) root.detectedXkbLayout = String(parsed.str)
        } catch (e) {
          // Leave the previous/default value — better than an empty layout.
        }
      }
    }
  }

  Process {
    id: xkbVariantProc
    command: ["hyprctl", "getoption", "input:kb_variant", "-j"]
    stdout: StdioCollector {
      waitForEnd: true
      onStreamFinished: {
        try {
          var parsed = JSON.parse(String(text || ""))
          root.detectedXkbVariant = String(parsed.str || "")
        } catch (e) {
          // Leave the previous/default value.
        }
      }
    }
  }

  // config.json carries the shared security key in plaintext, same
  // sensitivity as the Cameras plugin's nest-credentials.env — kept
  // unreadable to anyone but this user.
  Process {
    id: chmodProc
    command: ["chmod", "600", root.configPath]
  }

  Process {
    id: restartProc
    command: ["systemctl", "--user", "restart", root.serviceName]
    onExited: function(exitCode) {
      root.saveMessage = exitCode === 0 ? "Saved and restarted." : "Saved, but the restart failed — check `journalctl --user -u " + root.serviceName + "`."
      root.refreshServiceState()
    }
  }

  Process {
    id: startProc
    command: ["systemctl", "--user", "start", root.serviceName]
    onExited: function(exitCode) { root.refreshServiceState() }
  }

  Process {
    id: stopProc
    command: ["systemctl", "--user", "stop", root.serviceName]
    onExited: function(exitCode) { root.refreshServiceState() }
  }

  Process {
    id: serviceStateProc
    command: ["systemctl", "--user", "is-active", root.serviceName]
    stdout: StdioCollector {
      waitForEnd: true
      onStreamFinished: root.serviceState = String(text || "unknown").trim()
    }
  }

  Timer {
    interval: 5000
    running: true
    repeat: true
    onTriggered: root.refreshServiceState()
  }

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: "󰢹"
    slotSize: Style.bar.statusSlot
    active: root.popupOpen
    foreground: root.connected ? root.bar.barForeground : Qt.darker(root.bar.barForeground, 1.8)
    tooltipText: root.tooltipText

    onPressed: function(b) { root.popupOpen = !root.popupOpen }
  }

  // KeyboardPanel, not PopupCard — this popup has real text fields, and
  // PopupCard doesn't reliably route keyboard focus to a child TextField
  // (see the Cameras plugin's own note on this same gotcha).
  KeyboardPanel {
    id: popup
    anchorItem: root
    bar: root.bar
    owner: root
    open: root.popupOpen
    contentWidth: popup.fittedContentWidth(Style.space(root.popupWidth))
    contentHeight: popup.fittedContentHeight(column.implicitHeight)

    onOpenChanged: if (open) { root.refreshServiceState(); root.saveMessage = "" }

    Column {
      id: column
      anchors.fill: parent
      spacing: Style.space(10)

      Text {
        textFormat: Text.PlainText
        text: "MWB Bridge"
        color: root.bar.foreground
        font.family: root.bar.fontFamily
        font.pixelSize: Style.font.subtitle
        font.bold: true
      }

      Text {
        width: parent.width
        visible: root.setupStatus !== ""
        textFormat: Text.PlainText
        wrapMode: Text.WordWrap
        text: root.setupStatus
        color: root.setupStatus.indexOf("failed") !== -1 ? Color.urgent : Color.accent
        font.family: root.bar.fontFamily
        font.pixelSize: Style.font.bodySmall
      }

      Text {
        textFormat: Text.PlainText
        text: "Service: " + root.serviceState
        color: root.bar.foreground
        font.family: root.bar.fontFamily
        font.pixelSize: Style.font.bodySmall
      }

      Text {
        width: parent.width
        textFormat: Text.PlainText
        wrapMode: Text.WordWrap
        text: root.statusConnected ? ("Connected to " + root.statusPeer) : root.statusDetail
        color: root.statusConnected ? Color.accent : Qt.darker(root.bar.foreground, 1.4)
        font.family: root.bar.fontFamily
        font.pixelSize: Style.font.bodySmall
      }

      Row {
        width: parent.width
        spacing: Style.space(8)

        Button {
          text: "Start"
          foreground: root.bar.foreground
          horizontalPadding: Style.spacing.controlPaddingX
          verticalPadding: Style.space(4)
          enabled: root.serviceState !== "active"
          onClicked: startProc.running = true
        }

        Button {
          text: "Stop"
          foreground: root.bar.foreground
          horizontalPadding: Style.spacing.controlPaddingX
          verticalPadding: Style.space(4)
          enabled: root.serviceState === "active"
          onClicked: stopProc.running = true
        }

        Button {
          text: "Restart"
          foreground: root.bar.foreground
          horizontalPadding: Style.spacing.controlPaddingX
          verticalPadding: Style.space(4)
          onClicked: restartProc.running = true
        }
      }

      Rectangle {
        width: parent.width
        height: 1
        color: Qt.darker(root.bar.foreground, 4)
      }

      Text {
        textFormat: Text.PlainText
        text: "Settings"
        color: root.bar.foreground
        font.family: root.bar.fontFamily
        font.pixelSize: Style.font.bodySmall
        font.bold: true
      }

      Text {
        width: parent.width
        textFormat: Text.PlainText
        wrapMode: Text.WordWrap
        text: "Must exactly match a PowerToys Mouse Without Borders install on the paired Windows PC (same Security Key). This machine's name and ID below also need hand-adding to that PC's settings.json (MachineMatrixString + MachinePool) — the classic UI can't persist that reliably in this PowerToys version. See PROTOCOL.md in the mwb-omarchy-bridge repo."
        color: Qt.darker(root.bar.foreground, 1.4)
        font.family: root.bar.fontFamily
        font.pixelSize: Style.font.caption
      }

      Row {
        width: parent.width
        spacing: Style.space(6)

        TextField {
          id: keyField
          width: parent.width - showKeyButton.width - parent.spacing
          placeholderText: "Security Key (shared with Windows)"
          password: !root.showKey
        }

        Button {
          id: showKeyButton
          text: root.showKey ? "Hide" : "Show"
          foreground: root.bar.foreground
          horizontalPadding: Style.spacing.controlPaddingX
          verticalPadding: Style.space(4)
          onClicked: root.showKey = !root.showKey
        }
      }

      TextField {
        id: ipField
        width: parent.width
        placeholderText: "Windows IP or hostname:port (e.g. WINPC.local:15101)"
      }

      TextField {
        id: nameField
        width: parent.width
        placeholderText: "This machine's name (e.g. omarchy)"
      }

      Text {
        width: parent.width
        textFormat: Text.PlainText
        wrapMode: Text.WordWrap
        text: "Machine ID (for Windows' MachinePool entry): " + root.machineId
        color: Qt.darker(root.bar.foreground, 1.4)
        font.family: root.bar.fontFamily
        font.pixelSize: Style.font.caption
      }

      Text {
        width: parent.width
        visible: root.saveMessage !== ""
        textFormat: Text.PlainText
        wrapMode: Text.WordWrap
        text: root.saveMessage
        color: Color.accent
        font.family: root.bar.fontFamily
        font.pixelSize: Style.font.bodySmall
      }

      Button {
        text: "Save and restart"
        foreground: root.bar.foreground
        horizontalPadding: Style.spacing.controlPaddingX
        verticalPadding: Style.spacing.controlPaddingY
        enabled: keyField.text.trim() !== "" && ipField.text.trim() !== "" && nameField.text.trim() !== ""
        onClicked: root.saveSettings()
      }
    }
  }
}
