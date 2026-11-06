// SPDX-License-Identifier: GPL-3.0-or-later
#include "../src/catalogmodel.h"

#include <QJsonArray>
#include <QJsonObject>
#include <QtTest>

// Playlist edit mode leans on three filter-model behaviours: the "in
// playlist" filter, the list Select all acts on, and the Shift-click range.
class WallpaperFilterModelTest final : public QObject {
    Q_OBJECT

    static QJsonObject item(const QString &id, const QString &title, const QString &kind,
                            const QString &state = QStringLiteral("subscribed_installed")) {
        return QJsonObject{{QStringLiteral("workshop_id"), id},
                           {QStringLiteral("title"), title},
                           {QStringLiteral("kind"), kind},
                           {QStringLiteral("workshop_state"), state}};
    }

    CatalogModel m_source;
    WallpaperFilterModel m_filter;

private slots:
    void initTestCase() {
        m_filter.setSourceModel(&m_source);
        // The daemon delivers the catalog sorted by title; the view order is
        // 10, 20, 30, 40, 50.
        m_source.replaceFromCatalog(QJsonObject{{QStringLiteral("items"), QJsonArray{
            item(QStringLiteral("10"), QStringLiteral("A scene"), QStringLiteral("scene")),
            item(QStringLiteral("20"), QStringLiteral("B pending"), QStringLiteral("scene"),
                 QStringLiteral("subscribed_missing")),
            item(QStringLiteral("30"), QStringLiteral("C video"), QStringLiteral("video")),
            item(QStringLiteral("40"), QStringLiteral("D broken"), QStringLiteral("invalid")),
            item(QStringLiteral("50"), QStringLiteral("E local"), QStringLiteral("web"),
                 QStringLiteral("local")),
        }}});
        QCOMPARE(m_filter.rowCount(), 5);
    }

    void selectableIdsSkipWhatAPlaylistCannotPlay() {
        QCOMPARE(m_filter.selectableIds(),
                 (QStringList{QStringLiteral("10"), QStringLiteral("30"), QStringLiteral("50")}));
    }

    void selectableIdsFollowTheCurrentFilters() {
        m_filter.setKindFilter(QStringLiteral("video"));
        QCOMPARE(m_filter.selectableIds(), QStringList{QStringLiteral("30")});
        m_filter.setKindFilter(QStringLiteral("all"));
    }

    void rangeIsInclusiveEitherWayRoundAndSkipsUnplayable() {
        const QStringList expected{QStringLiteral("10"), QStringLiteral("30"), QStringLiteral("50")};
        QCOMPARE(m_filter.selectableIdsBetween(QStringLiteral("10"), QStringLiteral("50")), expected);
        QCOMPARE(m_filter.selectableIdsBetween(QStringLiteral("50"), QStringLiteral("10")), expected);
        QCOMPARE(m_filter.selectableIdsBetween(QStringLiteral("30"), QStringLiteral("30")),
                 QStringList{QStringLiteral("30")});
        // An end that is not shown gives nothing rather than a guess.
        QVERIFY(m_filter.selectableIdsBetween(QStringLiteral("10"), QStringLiteral("999")).isEmpty());
    }

    void playlistFilterShowsMembersOrTheRestAndTracksChanges() {
        m_filter.setPlaylistIds({QStringLiteral("30"), QStringLiteral("50")});
        QCOMPARE(m_filter.rowCount(), 5);  // filter off: membership alone hides nothing
        m_filter.setPlaylistFilter(QStringLiteral("in"));
        QCOMPARE(m_filter.selectableIds(), (QStringList{QStringLiteral("30"), QStringLiteral("50")}));
        m_filter.setPlaylistIds({QStringLiteral("50")});
        QCOMPARE(m_filter.rowCount(), 1);
        // The complement: everything not yet in the playlist.
        m_filter.setPlaylistFilter(QStringLiteral("out"));
        QCOMPARE(m_filter.rowCount(), 4);
        QCOMPARE(m_filter.selectableIds(), (QStringList{QStringLiteral("10"), QStringLiteral("30")}));
        m_filter.setPlaylistIds({QStringLiteral("50"), QStringLiteral("10")});
        QCOMPARE(m_filter.selectableIds(), QStringList{QStringLiteral("30")});
        m_filter.setPlaylistFilter(QStringLiteral("bogus"));  // unknown values mean off
        QCOMPARE(m_filter.playlistFilter(), QStringLiteral("all"));
        QCOMPARE(m_filter.rowCount(), 5);
    }

    // W5: the daemon's per-item update flag reaches the role, the count and
    // the details lookup; it is ignored on anything but an installed
    // subscription, and absent (older daemon) reads as false.
    void updateAvailableReachesRoleCountAndLookup() {
        CatalogModel model;
        QSignalSpy stats(&model, &CatalogModel::statsChanged);
        auto flagged = [](QJsonObject object, bool value) {
            object.insert(QStringLiteral("workshop_update_available"), value);
            return object;
        };
        model.replaceFromCatalog(QJsonObject{{QStringLiteral("items"), QJsonArray{
            flagged(item(QStringLiteral("1"), QStringLiteral("A"), QStringLiteral("scene")), true),
            flagged(item(QStringLiteral("2"), QStringLiteral("B"), QStringLiteral("scene")), false),
            item(QStringLiteral("3"), QStringLiteral("C"), QStringLiteral("scene")),
            flagged(item(QStringLiteral("4"), QStringLiteral("D"), QStringLiteral("scene"),
                         QStringLiteral("subscribed_missing")), true),
            flagged(item(QStringLiteral("5"), QStringLiteral("E"), QStringLiteral("scene"),
                         QStringLiteral("local")), true),
        }}});
        QCOMPARE(stats.count(), 1);
        QCOMPARE(model.updateAvailableCount(), 1);
        QCOMPARE(model.subscribedCount(), 3);
        const auto role = [&model](int row) {
            return model.data(model.index(row), CatalogModel::WorkshopUpdateAvailableRole).toBool();
        };
        QVERIFY(role(0));
        QVERIFY(!role(1));
        QVERIFY(!role(2));
        QVERIFY(!role(3));
        QVERIFY(!role(4));
        QVERIFY(model.roleNames().values().contains("workshopUpdateAvailable"));
        const auto entry = model.itemById(QStringLiteral("1"));
        QCOMPARE(entry.value(QStringLiteral("updateAvailable")).toBool(), true);
        QCOMPARE(entry.value(QStringLiteral("workshopState")).toString(),
                 QStringLiteral("subscribed_installed"));
        const auto gone = model.itemById(QStringLiteral("999"));
        QCOMPARE(gone.value(QStringLiteral("found")).toBool(), false);
        QCOMPARE(gone.value(QStringLiteral("updateAvailable")).toBool(), false);
        QCOMPARE(gone.value(QStringLiteral("workshopState")).toString(), QString());

        // Steam finished the update: the flag and the count drop.
        model.replaceFromCatalog(QJsonObject{{QStringLiteral("items"), QJsonArray{
            flagged(item(QStringLiteral("1"), QStringLiteral("A"), QStringLiteral("scene")), false),
        }}});
        QCOMPARE(model.updateAvailableCount(), 0);
        QVERIFY(!role(0));
    }
};

QTEST_MAIN(WallpaperFilterModelTest)
#include "wallpaperfiltermodeltest.moc"
