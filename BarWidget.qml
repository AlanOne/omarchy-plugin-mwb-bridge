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

  function refreshServiceState() { serviceStateProc.running = true }

  Component.onCompleted: {
    if (root.machineId === 0) root.machineId = Math.floor(Math.random() * 4294967295)
    root.refreshServiceState()
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
      machine_id: root.machineId
    }
    configFile.setText(JSON.stringify(cfg, null, 2) + "\n")
    chmodProc.running = true
    restartProc.running = true
    root.saveMessage = "Saved — restarting service..."
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
    text: "󰒃"
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
