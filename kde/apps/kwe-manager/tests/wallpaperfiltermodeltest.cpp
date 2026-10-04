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

    void playlistOnlyShowsMembersAndTracksChanges() {
        m_filter.setPlaylistIds({QStringLiteral("30"), QStringLiteral("50")});
        QCOMPARE(m_filter.rowCount(), 5);  // filter off: membership alone hides nothing
        m_filter.setPlaylistOnly(true);
        QCOMPARE(m_filter.selectableIds(), (QStringList{QStringLiteral("30"), QStringLiteral("50")}));
        m_filter.setPlaylistIds({QStringLiteral("50")});
        QCOMPARE(m_filter.rowCount(), 1);
        m_filter.setPlaylistOnly(false);
        QCOMPARE(m_filter.rowCount(), 5);
    }
};

QTEST_MAIN(WallpaperFilterModelTest)
#include "wallpaperfiltermodeltest.moc"
