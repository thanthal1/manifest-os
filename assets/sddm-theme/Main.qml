// Manifest OS — SDDM login theme.
// Colours/background come from theme.conf via the `config` object SDDM
// injects (config.AccentColor, config.Background, ...); a manifest can
// override theme.conf per-install (see desktop::configure_display_manager /
// the `files` block in flagship examples) without touching this file.
import QtQuick
import QtQuick.Controls
import QtQuick.Window

Item {
    id: root
    width: Screen.width
    height: Screen.height

    readonly property color accentColor: config.AccentColor ? config.AccentColor : "#7aa2f7"
    readonly property color panelColor: config.PanelColor ? config.PanelColor : "#141414"
    readonly property color fgColor: "#f2f2f2"
    readonly property string bgPath: config.Background ? config.Background : ""
    readonly property string fontFamily: config.FontFamily ? config.FontFamily : "sans-serif"

    // Plain dark base first, so there's never a blank/invisible screen even if
    // the wallpaper image fails to load (missing file, permissions, ...).
    Rectangle {
        anchors.fill: parent
        color: "#0c0c10"
    }
    Image {
        anchors.fill: parent
        source: root.bgPath !== "" ? "file://" + root.bgPath : ""
        fillMode: Image.PreserveAspectCrop
        visible: root.bgPath !== "" && status === Image.Ready
        asynchronous: true
    }
    Rectangle {
        anchors.fill: parent
        color: "#000000"
        opacity: 0.35
    }

    QtObject {
        id: clockTick
        property date now: new Date()
    }
    Timer {
        interval: 1000
        running: true
        repeat: true
        onTriggered: clockTick.now = new Date()
    }

    Column {
        anchors.horizontalCenter: parent.horizontalCenter
        y: parent.height * 0.20
        spacing: 4

        Text {
            anchors.horizontalCenter: parent.horizontalCenter
            text: Qt.formatTime(clockTick.now, "hh:mm")
            font.pixelSize: 72
            font.family: root.fontFamily
            color: root.fgColor
        }
        Text {
            anchors.horizontalCenter: parent.horizontalCenter
            text: Qt.formatDate(clockTick.now, "dddd, MMMM d")
            font.pixelSize: 18
            font.family: root.fontFamily
            color: root.fgColor
            opacity: 0.75
        }
    }

    Rectangle {
        id: card
        width: 360
        height: form.implicitHeight + 56
        radius: 18
        color: Qt.rgba(root.panelColor.r, root.panelColor.g, root.panelColor.b, 0.80)
        border.width: 2
        border.color: Qt.rgba(root.accentColor.r, root.accentColor.g, root.accentColor.b, 0.65)
        anchors.horizontalCenter: parent.horizontalCenter
        y: parent.height * 0.46

        Column {
            id: form
            anchors.centerIn: parent
            width: parent.width - 56
            spacing: 12

            ComboBox {
                id: userBox
                width: parent.width
                model: userModel
                textRole: "name"
                currentIndex: userModel.lastIndex >= 0 ? userModel.lastIndex : 0
            }

            TextField {
                id: passwordField
                width: parent.width
                echoMode: TextInput.Password
                placeholderText: "Password"
                focus: true
                onAccepted: loginButton.clicked()
            }

            Text {
                id: errorText
                width: parent.width
                text: ""
                color: "#ff6b6b"
                visible: text !== ""
                wrapMode: Text.WordWrap
                horizontalAlignment: Text.AlignHCenter
                font.pixelSize: 12
                font.family: root.fontFamily
            }

            Button {
                id: loginButton
                width: parent.width
                text: "Log In"
                onClicked: sddm.login(userBox.currentText, passwordField.text, sessionBox.currentIndex)
            }

            ComboBox {
                id: sessionBox
                width: parent.width
                model: sessionModel
                textRole: "name"
                currentIndex: sessionModel.lastIndex >= 0 ? sessionModel.lastIndex : 0
            }
        }
    }

    // Basic Unicode symbols, not Nerd Font glyphs — a login screen can't
    // assume a Nerd Font is the active fallback font.
    Row {
        anchors.bottom: parent.bottom
        anchors.right: parent.right
        anchors.margins: 28
        spacing: 26

        Text {
            text: "⏻"
            font.pixelSize: 26
            color: root.fgColor
            visible: sddm.canPowerOff
            MouseArea { anchors.fill: parent; cursorShape: Qt.PointingHandCursor; onClicked: sddm.powerOff() }
        }
        Text {
            text: "↻"
            font.pixelSize: 26
            color: root.fgColor
            visible: sddm.canReboot
            MouseArea { anchors.fill: parent; cursorShape: Qt.PointingHandCursor; onClicked: sddm.reboot() }
        }
        Text {
            text: "⏾"
            font.pixelSize: 26
            color: root.fgColor
            visible: sddm.canSuspend
            MouseArea { anchors.fill: parent; cursorShape: Qt.PointingHandCursor; onClicked: sddm.suspend() }
        }
    }

    Connections {
        target: sddm
        function onLoginFailed() {
            errorText.text = "Incorrect password — try again"
            passwordField.text = ""
            passwordField.forceActiveFocus()
        }
    }

    Component.onCompleted: passwordField.forceActiveFocus()
}
