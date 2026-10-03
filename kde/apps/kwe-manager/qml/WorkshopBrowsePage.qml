// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kwe

// Browse the Steam Workshop (needs a Steam sign-in). Everything shown here
// is remote and untrusted: titles and descriptions are rendered as plain
// text. Installed content stays available in the Subscribed view whatever
// state this page is in.
Kirigami.Page {
    id: browsePage

    // Set by the host when this view is the visible one. The first time it
    // turns true the account status is fetched — never at app start.
    property bool active: false
    property bool statusRequested: false

    readonly property bool signedIn: workshopClient.authState === WorkshopClient.SignedIn
    readonly property int catalogState: remoteCatalog.state

    padding: 0

    readonly property var sortOptions: [
        { label: qsTr("Trending today"), value: "trend_day" },
        { label: qsTr("Trending this week"), value: "trend_week" },
        { label: qsTr("Trending this month"), value: "trend_month" },
        { label: qsTr("Trending this year"), value: "trend_year" },
        { label: qsTr("Most recent"), value: "recent" },
        { label: qsTr("Most subscribed"), value: "most_subscribed" },
        { label: qsTr("Top rated"), value: "top_rated" }
    ]

    // Selected item for the detail sheet (a snapshot of the model row,
    // refreshed on dataChanged so the subscribe state stays live).
    property var detail: ({})
    property string detailDescription: ""
    property bool detailLoading: false
    property string detailError: ""

    onActiveChanged: {
        if (active && !statusRequested) {
            statusRequested = true
            workshopClient.refreshAuthStatus()
        }
    }

    function applyFilters() {
        const tags = []
        if (typeBox.currentValue !== "")
            tags.push(typeBox.currentValue)
        const extra = tagField.text.trim()
        if (extra !== "")
            tags.push(extra)
        remoteCatalog.searchText = searchField.text.trim()
        remoteCatalog.sort = sortBox.currentValue
        remoteCatalog.tags = tags
        if (signedIn)
            remoteCatalog.refresh()
    }

    function openDetail(workshopId) {
        detail = remoteCatalog.itemById(workshopId)
        detailDescription = ""
        detailError = ""
        detailLoading = true
        remoteCatalog.details(workshopId)
        detailSheet.open()
    }

    function formatSize(bytes) {
        if (bytes <= 0)
            return ""
        const mib = bytes / (1024 * 1024)
        return mib >= 1024 ? qsTr("%1 GiB").arg((mib / 1024).toFixed(1))
                           : qsTr("%1 MiB").arg(mib.toFixed(mib < 10 ? 1 : 0))
    }

    Connections {
        target: workshopClient
        function onAuthStateChanged() {
            if (workshopClient.authState === WorkshopClient.SignedIn
                    && remoteCatalog.state !== RemoteCatalogModel.Loading)
                remoteCatalog.refresh()
        }
    }

    Connections {
        target: remoteCatalog
        function onDetailsReady(workshopId, description) {
            if (workshopId === browsePage.detail.workshopId) {
                browsePage.detailDescription = description
                browsePage.detailLoading = false
            }
        }
        function onDetailsFailed(workshopId, message) {
            if (workshopId === browsePage.detail.workshopId) {
                browsePage.detailError = message
                browsePage.detailLoading = false
            }
        }
        function onDataChanged() {
            if (browsePage.detail.workshopId !== undefined)
                browsePage.detail = remoteCatalog.itemById(browsePage.detail.workshopId)
        }
    }

    Timer {
        id: searchDebounce
        interval: 400
        onTriggered: browsePage.applyFilters()
    }

    header: Controls.ToolBar {
        contentItem: RowLayout {
            spacing: Kirigami.Units.smallSpacing

            Kirigami.SearchField {
                id: searchField
                Layout.fillWidth: true
                Layout.maximumWidth: Kirigami.Units.gridUnit * 24
                enabled: browsePage.signedIn
                placeholderText: qsTr("Search the Workshop…")
                Accessible.name: qsTr("Search the Steam Workshop")
                onTextChanged: searchDebounce.restart()
                onAccepted: {
                    searchDebounce.stop()
                    browsePage.applyFilters()
                }
            }

            Controls.ComboBox {
                id: sortBox
                enabled: browsePage.signedIn
                textRole: "label"
                valueRole: "value"
                model: browsePage.sortOptions
                currentIndex: 1
                Accessible.name: qsTr("Sort Workshop items")
                onActivated: browsePage.applyFilters()
            }

            Controls.ComboBox {
                id: typeBox
                enabled: browsePage.signedIn
                textRole: "label"
                valueRole: "value"
                model: [
                    { label: qsTr("All types"), value: "" },
                    { label: qsTr("Scene"), value: "Scene" },
                    { label: qsTr("Video"), value: "Video" },
                    { label: qsTr("Web"), value: "Web" }
                ]
                Accessible.name: qsTr("Filter by wallpaper type")
                onActivated: browsePage.applyFilters()
            }

            Controls.TextField {
                id: tagField
                Layout.preferredWidth: Kirigami.Units.gridUnit * 9
                enabled: browsePage.signedIn
                placeholderText: qsTr("Tag, e.g. Anime")
                Accessible.name: qsTr("Filter by Workshop tag")
                onAccepted: browsePage.applyFilters()
            }

            Item { Layout.fillWidth: true }

            Kirigami.Icon {
                visible: browsePage.signedIn
                implicitWidth: Kirigami.Units.iconSizes.small
                implicitHeight: implicitWidth
                source: "user-online-symbolic"
            }
            Controls.Label {
                visible: browsePage.signedIn
                text: qsTr("Signed in as %1").arg(workshopClient.accountName)
                textFormat: Text.PlainText
                elide: Text.ElideRight
                Layout.maximumWidth: Kirigami.Units.gridUnit * 14
            }
            Controls.ToolButton {
                visible: browsePage.signedIn
                text: qsTr("Sign out")
                icon.name: "system-log-out-symbolic"
                display: Controls.AbstractButton.TextBesideIcon
                Accessible.name: qsTr("Sign out of Steam")
                onClicked: workshopClient.signOut()
            }
        }
    }

    // Not signed in (or still checking): the account card replaces the grid.
    Flickable {
        id: accountView
        anchors.fill: parent
        visible: !browsePage.signedIn
        contentHeight: accountColumn.implicitHeight + Kirigami.Units.gridUnit * 2
        clip: true

        ColumnLayout {
            id: accountColumn
            width: Math.min(accountView.width - Kirigami.Units.gridUnit * 2,
                            Kirigami.Units.gridUnit * 30)
            x: (accountView.width - width) / 2
            y: Kirigami.Units.gridUnit
            spacing: Kirigami.Units.largeSpacing

            SteamSignInCard {
                Layout.fillWidth: true
            }
        }
    }

    ColumnLayout {
        anchors.fill: parent
        spacing: 0
        visible: browsePage.signedIn

        Kirigami.InlineMessage {
            Layout.fillWidth: true
            visible: remoteCatalog.notice !== ""
            type: Kirigami.MessageType.Warning
            text: remoteCatalog.notice
        }
        Kirigami.InlineMessage {
            Layout.fillWidth: true
            // A failed "load more" keeps the rows on screen.
            visible: browsePage.catalogState === RemoteCatalogModel.Ready
                && remoteCatalog.errorMessage !== ""
            type: Kirigami.MessageType.Error
            text: remoteCatalog.errorMessage
            actions: [
                Kirigami.Action {
                    text: qsTr("Try again")
                    icon.name: "view-refresh-symbolic"
                    onTriggered: remoteCatalog.loadMore()
                }
            ]
        }

        GridView {
            id: grid
            Layout.fillWidth: true
            Layout.fillHeight: true
            clip: true
            model: remoteCatalog
            cellWidth: Math.max(Kirigami.Units.gridUnit * 13,
                width / Math.max(1, Math.floor(width / (Kirigami.Units.gridUnit * 13))))
            cellHeight: Kirigami.Units.gridUnit * 11
            keyNavigationEnabled: true
            activeFocusOnTab: true
            leftMargin: Kirigami.Units.largeSpacing
            rightMargin: Kirigami.Units.largeSpacing
            Accessible.name: qsTr("Workshop items")

            // Near the end: ask for the next page. Appending rows does not
            // move contentY, so the scroll position survives.
            function maybeLoadMore() {
                if (remoteCatalog.hasMore && !remoteCatalog.loadingMore
                        && contentY + height > contentHeight - cellHeight * 2)
                    remoteCatalog.loadMore()
            }
            onContentYChanged: maybeLoadMore()
            onContentHeightChanged: maybeLoadMore()

            delegate: RemoteItemCard {
                id: remoteCard
                cellWidth: grid.cellWidth
                cellHeight: grid.cellHeight
                onOpened: workshopId => browsePage.openDetail(workshopId)
            }

            footer: Item {
                width: grid.width
                height: remoteCatalog.loadingMore ? Kirigami.Units.gridUnit * 3 : 0
                Controls.BusyIndicator {
                    anchors.centerIn: parent
                    running: remoteCatalog.loadingMore
                    visible: running
                    Accessible.name: qsTr("Loading more Workshop items")
                }
            }

            Kirigami.PlaceholderMessage {
                anchors.centerIn: parent
                width: Math.min(parent.width - Kirigami.Units.gridUnit * 2,
                                Kirigami.Units.gridUnit * 28)
                visible: browsePage.catalogState !== RemoteCatalogModel.Ready
                text: {
                    switch (browsePage.catalogState) {
                    case RemoteCatalogModel.Loading:
                    case RemoteCatalogModel.Idle: return qsTr("Loading Workshop items…")
                    case RemoteCatalogModel.Empty: return qsTr("No Workshop items match")
                    case RemoteCatalogModel.Offline: return qsTr("Steam is unreachable")
                    case RemoteCatalogModel.RateLimited: return qsTr("Steam is limiting requests")
                    case RemoteCatalogModel.SignedOut: return qsTr("You are signed out")
                    default: return qsTr("Could not load the Workshop")
                    }
                }
                explanation: {
                    switch (browsePage.catalogState) {
                    case RemoteCatalogModel.Loading:
                    case RemoteCatalogModel.Idle:
                        return qsTr("Your installed wallpapers stay available in the Subscribed view.")
                    case RemoteCatalogModel.Empty:
                        return qsTr("Change the search, sort or filters.")
                    default: return remoteCatalog.errorMessage
                    }
                }
                icon.name: {
                    switch (browsePage.catalogState) {
                    case RemoteCatalogModel.Loading:
                    case RemoteCatalogModel.Idle: return "view-refresh-symbolic"
                    case RemoteCatalogModel.Empty: return "edit-find-symbolic"
                    case RemoteCatalogModel.Offline: return "network-disconnect-symbolic"
                    case RemoteCatalogModel.RateLimited: return "chronometer-symbolic"
                    default: return "dialog-error-symbolic"
                    }
                }
                helpfulAction: Kirigami.Action {
                    visible: browsePage.catalogState === RemoteCatalogModel.Offline
                        || browsePage.catalogState === RemoteCatalogModel.RateLimited
                        || browsePage.catalogState === RemoteCatalogModel.Failed
                    text: qsTr("Retry")
                    icon.name: "view-refresh-symbolic"
                    onTriggered: remoteCatalog.refresh()
                }
            }
        }
    }

    Kirigami.OverlaySheet {
        id: detailSheet
        // Remote, untrusted text: never let it be parsed as markup.
        header: Kirigami.Heading {
            text: browsePage.detail.title !== undefined ? browsePage.detail.title : ""
            textFormat: Text.PlainText
            elide: Text.ElideRight
            maximumLineCount: 1
        }

        ColumnLayout {
            spacing: Kirigami.Units.largeSpacing

            Item {
                Layout.fillWidth: true
                Layout.preferredHeight: Kirigami.Units.gridUnit * 14
                Image {
                    id: detailImage
                    anchors.fill: parent
                    source: browsePage.detail.previewUrl !== undefined ? browsePage.detail.previewUrl : ""
                    fillMode: Image.PreserveAspectFit
                    asynchronous: true
                    cache: true
                    sourceSize.width: 768
                    sourceSize.height: 432
                    visible: status === Image.Ready
                }
                Kirigami.Icon {
                    anchors.centerIn: parent
                    width: Kirigami.Units.iconSizes.huge
                    height: width
                    source: "preferences-desktop-wallpaper-symbolic"
                    opacity: 0.55
                    visible: detailImage.status !== Image.Ready
                }
            }

            Controls.Label {
                Layout.fillWidth: true
                wrapMode: Text.WordWrap
                textFormat: Text.PlainText
                text: {
                    const d = browsePage.detail
                    if (d.kind === undefined)
                        return ""
                    const parts = []
                    if (d.kind !== "")
                        parts.push(d.kind.charAt(0).toUpperCase() + d.kind.slice(1))
                    const size = browsePage.formatSize(d.fileSize)
                    if (size !== "")
                        parts.push(size)
                    parts.push(qsTr("%1 subscribers").arg(Number(d.subscriptions).toLocaleString(Qt.locale(), "f", 0)))
                    return parts.join(" · ")
                }
            }

            Controls.Label {
                Layout.fillWidth: true
                visible: browsePage.detail.tags !== undefined && browsePage.detail.tags.length > 0
                wrapMode: Text.WordWrap
                textFormat: Text.PlainText
                text: browsePage.detail.tags !== undefined
                    ? qsTr("Tags: %1").arg(browsePage.detail.tags.join(", ")) : ""
            }

            RowLayout {
                Layout.fillWidth: true
                visible: browsePage.detailLoading
                Controls.BusyIndicator { running: browsePage.detailLoading }
                Controls.Label { text: qsTr("Loading description…") }
            }
            Controls.Label {
                Layout.fillWidth: true
                // Untrusted remote text: never interpreted as markup.
                textFormat: Text.PlainText
                wrapMode: Text.WordWrap
                visible: !browsePage.detailLoading && browsePage.detailError === ""
                text: browsePage.detailDescription !== ""
                    ? browsePage.detailDescription
                    : (browsePage.detail.shortDescription !== undefined ? browsePage.detail.shortDescription : "")
            }
            Kirigami.InlineMessage {
                Layout.fillWidth: true
                visible: browsePage.detailError !== ""
                type: Kirigami.MessageType.Warning
                text: browsePage.detailError
            }

            RowLayout {
                Layout.fillWidth: true
                spacing: Kirigami.Units.smallSpacing

                Controls.Button {
                    visible: browsePage.signedIn && browsePage.detail.subscribed !== true
                    enabled: browsePage.detail.subscriptionPending !== true
                    text: browsePage.detail.subscriptionPending === true ? qsTr("Subscribing…") : qsTr("Subscribe")
                    icon.name: "list-add-symbolic"
                    onClicked: remoteCatalog.subscribe(browsePage.detail.workshopId)
                }
                Controls.Button {
                    visible: browsePage.signedIn && browsePage.detail.subscribed === true
                    enabled: browsePage.detail.subscriptionPending !== true
                    text: browsePage.detail.subscriptionPending === true ? qsTr("Unsubscribing…") : qsTr("Unsubscribe")
                    icon.name: "list-remove-symbolic"
                    onClicked: remoteCatalog.unsubscribe(browsePage.detail.workshopId)
                }
                Controls.Button {
                    visible: !browsePage.signedIn
                    text: qsTr("Open in Steam to subscribe")
                    icon.name: "internet-services-symbolic"
                    onClicked: workshopClient.openItem(browsePage.detail.workshopId)
                }
                Controls.Button {
                    text: qsTr("Open in Steam Workshop")
                    icon.name: "internet-services-symbolic"
                    onClicked: workshopClient.openItem(browsePage.detail.workshopId)
                    Accessible.description: qsTr("Open the canonical Workshop item in the Steam client")
                }
            }
            Kirigami.InlineMessage {
                Layout.fillWidth: true
                visible: workshopClient.errorMessage !== ""
                type: Kirigami.MessageType.Error
                text: workshopClient.errorMessage
            }
        }
    }
}
