// SPDX-License-Identifier: GPL-3.0-or-later
#include "../src/rendererdefaults.h"
#include "../src/settingsclient.h"
#include "stubsettingsdaemon.h"

#include <QCoreApplication>
#include <QSettings>
#include <QTemporaryDir>
#include <QtTest>

class RendererDefaultsTest final : public QObject {
    Q_OBJECT

private slots:
    void initTestCase() {
        QVERIFY(m_settingsRoot.isValid());
        QCoreApplication::setOrganizationName(QStringLiteral("KDEWallpaperEngineTests"));
        QCoreApplication::setApplicationName(QStringLiteral("RendererDefaults"));
        QSettings::setDefaultFormat(QSettings::IniFormat);
        QSettings::setPath(QSettings::IniFormat, QSettings::UserScope, m_settingsRoot.path());
        m_socketPath = m_settingsRoot.path() + QStringLiteral("/daemon.sock");
        QVERIFY(m_daemon.listen(m_socketPath));
    }

    void init() {
        QSettings settings;
        settings.clear();
        settings.sync();
        m_daemon.reset();
    }

    void forwardsToTheDaemonRecordAndBumpsRevision() {
        m_daemon.rendererDefaults.insert(QStringLiteral("scene"),
                                         StubSettingsDaemon::kind(60, QStringLiteral("fill")));
        SettingsClient client(m_socketPath);
        RendererDefaults defaults(&client);
        QSignalSpy changedSpy(&defaults, &RendererDefaults::changed);
        // Built-ins until the daemon answers.
        QCOMPARE(defaults.fps(QStringLiteral("scene")), 30);
        QTRY_VERIFY(client.loaded());
        QVERIFY(changedSpy.count() >= 1);
        const int revision = defaults.revision();
        QCOMPARE(defaults.fps(QStringLiteral("scene")), 60);
        QCOMPARE(defaults.scaling(QStringLiteral("scene")), QStringLiteral("fill"));
        QCOMPARE(defaults.fps(QStringLiteral("video")), 30);

        defaults.setFps(QStringLiteral("video"), 15);
        defaults.setScaling(QStringLiteral("web"), QStringLiteral("stretch"));
        QTRY_COMPARE(m_daemon.sets, 2);
        QTRY_VERIFY(!client.busy());
        QVERIFY(defaults.revision() > revision);
        QCOMPARE(defaults.fps(QStringLiteral("video")), 15);
        QCOMPARE(defaults.scaling(QStringLiteral("web")), QStringLiteral("stretch"));
        QCOMPARE(m_daemon.rendererDefaults.value(QStringLiteral("video")).toObject().value(QStringLiteral("fps")).toInt(), 15);

        // Invalid input never reaches the daemon.
        defaults.setFps(QStringLiteral("scene"), 0);
        defaults.setFps(QStringLiteral("scene"), 241);
        defaults.setFps(QStringLiteral("toaster"), 30);
        defaults.setScaling(QStringLiteral("scene"), QStringLiteral("sideways"));
        QCOMPARE(m_daemon.sets, 2);
        QCOMPARE(defaults.fps(QStringLiteral("toaster")), 30);
    }

    void migratesLegacyQSettingsOnceWhenTheDaemonHoldsTheBuiltIns() {
        QSettings settings;
        settings.setValue(QStringLiteral("renderer-defaults/scene/fps"), 15);
        settings.setValue(QStringLiteral("renderer-defaults/scene/scaling"), QStringLiteral("stretch"));
        settings.setValue(QStringLiteral("renderer-defaults/video/fps"), 24);
        // Out-of-range legacy values are dropped, not migrated.
        settings.setValue(QStringLiteral("renderer-defaults/web/fps"), 100000);
        settings.setValue(QStringLiteral("renderer-defaults/web/scaling"), QStringLiteral("huge"));
        settings.sync();

        SettingsClient client(m_socketPath);
        RendererDefaults defaults(&client);
        QTRY_VERIFY(client.loaded());
        QTRY_COMPARE(m_daemon.sets, 3);
        QTRY_VERIFY(!client.busy());
        QCOMPARE(m_daemon.rendererDefaults.value(QStringLiteral("scene")).toObject().value(QStringLiteral("fps")).toInt(), 15);
        QCOMPARE(m_daemon.rendererDefaults.value(QStringLiteral("scene")).toObject().value(QStringLiteral("scaling")).toString(),
                 QStringLiteral("stretch"));
        QCOMPARE(m_daemon.rendererDefaults.value(QStringLiteral("video")).toObject().value(QStringLiteral("fps")).toInt(), 24);
        QCOMPARE(m_daemon.rendererDefaults.value(QStringLiteral("web")).toObject().value(QStringLiteral("fps")).toInt(), 30);
        QCOMPARE(defaults.fps(QStringLiteral("scene")), 15);
        // The legacy keys are gone, so a later start migrates nothing.
        QSettings after;
        QVERIFY(!after.childGroups().contains(QStringLiteral("renderer-defaults")));
        SettingsClient again(m_socketPath);
        RendererDefaults twice(&again);
        QTRY_VERIFY(again.loaded());
        QTest::qWait(50);
        QCOMPARE(m_daemon.sets, 3);
    }

    void legacyValuesNeverOverwriteAnEditedDaemonRecord() {
        QSettings settings;
        settings.setValue(QStringLiteral("renderer-defaults/scene/fps"), 15);
        settings.sync();
        m_daemon.rendererDefaults.insert(QStringLiteral("video"),
                                         StubSettingsDaemon::kind(60, QStringLiteral("aspect")));

        SettingsClient client(m_socketPath);
        RendererDefaults defaults(&client);
        QTRY_VERIFY(client.loaded());
        QTest::qWait(50);
        QCOMPARE(m_daemon.sets, 0);
        QCOMPARE(defaults.fps(QStringLiteral("scene")), 30);
        QSettings after;
        QVERIFY(!after.childGroups().contains(QStringLiteral("renderer-defaults")));
    }

private:
    QTemporaryDir m_settingsRoot;
    StubSettingsDaemon m_daemon;
    QString m_socketPath;
};

QTEST_GUILESS_MAIN(RendererDefaultsTest)
#include "rendererdefaultstest.moc"
