// SPDX-License-Identifier: GPL-3.0-or-later
#include "../src/settingsclient.h"
#include "stubsettingsdaemon.h"

#include <QJsonObject>
#include <QTemporaryDir>
#include <QtTest>

class SettingsClientTest final : public QObject {
    Q_OBJECT

private slots:
    void initTestCase() {
        QVERIFY(m_root.isValid());
        m_socketPath = m_root.path() + QStringLiteral("/daemon.sock");
        QVERIFY(m_daemon.listen(m_socketPath));
    }

    void init() { m_daemon.reset(); }

    void rendererDefaultsLoadAndPatchOneKindAtATime() {
        m_daemon.rendererDefaults.insert(QStringLiteral("video"),
                                         StubSettingsDaemon::kind(60, QStringLiteral("stretch")));
        SettingsClient client(m_socketPath);
        QVERIFY(client.rendererDefaultsAreBuiltIn());
        QTRY_VERIFY(client.loaded());
        QCOMPARE(client.rendererFps(QStringLiteral("video")), 60);
        QCOMPARE(client.rendererScaling(QStringLiteral("video")), QStringLiteral("stretch"));
        QCOMPARE(client.rendererFps(QStringLiteral("scene")), 30);
        QCOMPARE(client.rendererScaling(QStringLiteral("web")), QStringLiteral("aspect"));
        QVERIFY(!client.rendererDefaultsAreBuiltIn());
        // Unknown kinds read as the built-ins and are never written.
        QCOMPARE(client.rendererFps(QStringLiteral("toaster")), 30);
        client.setRendererFps(QStringLiteral("toaster"), 15);
        client.setRendererFps(QStringLiteral("scene"), 0);
        client.setRendererFps(QStringLiteral("scene"), 241);
        client.setRendererScaling(QStringLiteral("scene"), QStringLiteral("sideways"));
        QCOMPARE(m_daemon.sets, 0);

        client.setRendererFps(QStringLiteral("scene"), 15);
        // Optimistic immediately, authoritative after the round trip; the
        // patch names only the kind and field that changed.
        QCOMPARE(client.rendererFps(QStringLiteral("scene")), 15);
        QTRY_COMPARE(m_daemon.sets, 1);
        QTRY_VERIFY(!client.busy());
        QCOMPARE(m_daemon.lastSetParams.keys(), QStringList{QStringLiteral("renderer_defaults")});
        const auto patch = m_daemon.lastSetParams.value(QStringLiteral("renderer_defaults")).toObject();
        QCOMPARE(patch.keys(), QStringList{QStringLiteral("scene")});
        QCOMPARE(patch.value(QStringLiteral("scene")).toObject().keys(), QStringList{QStringLiteral("fps")});
        QCOMPARE(m_daemon.rendererDefaults.value(QStringLiteral("scene")).toObject().value(QStringLiteral("fps")).toInt(), 15);
        QCOMPARE(m_daemon.rendererDefaults.value(QStringLiteral("video")).toObject().value(QStringLiteral("fps")).toInt(), 60);

        client.setRendererScaling(QStringLiteral("web"), QStringLiteral("fill"));
        QTRY_COMPARE(m_daemon.sets, 2);
        QTRY_VERIFY(!client.busy());
        QCOMPARE(client.rendererScaling(QStringLiteral("web")), QStringLiteral("fill"));
        QCOMPARE(m_daemon.rendererDefaults.value(QStringLiteral("web")).toObject().value(QStringLiteral("scaling")).toString(),
                 QStringLiteral("fill"));
        QVERIFY(client.errorMessage().isEmpty());
    }

    void pauseWhenDisplayOffPatchesOnlyItsOwnField() {
        SettingsClient client(m_socketPath);
        QTRY_VERIFY(client.loaded());
        QVERIFY(!client.pauseWhenDisplayOff());

        client.setPauseWhenDisplayOff(true);
        QVERIFY(client.pauseWhenDisplayOff());
        QTRY_COMPARE(m_daemon.sets, 1);
        QTRY_VERIFY(!client.busy());
        QVERIFY(m_daemon.pauseWhenDisplayOff);
        QCOMPARE(m_daemon.lastSetParams.keys(),
                 QStringList{QStringLiteral("pause_when_display_off")});
        QVERIFY(!m_daemon.pauseWhenCovered);
        QVERIFY(m_daemon.audioOutput);
        QVERIFY(client.errorMessage().isEmpty());
    }

    void pauseWhenCoveredPatchesOnlyItsOwnField() {
        m_daemon.pauseWhenCovered = true;
        SettingsClient client(m_socketPath);
        QTRY_VERIFY(client.loaded());
        QVERIFY(client.pauseWhenCovered());
        QVERIFY(client.audioOutput());

        client.setPauseWhenCovered(false);
        QVERIFY(!client.pauseWhenCovered());
        QTRY_COMPARE(m_daemon.sets, 1);
        QTRY_VERIFY(!client.busy());
        QVERIFY(!m_daemon.pauseWhenCovered);
        QCOMPARE(m_daemon.lastSetParams.keys(), QStringList{QStringLiteral("pause_when_covered")});
        QVERIFY(m_daemon.audioOutput);
        QVERIFY(client.errorMessage().isEmpty());
    }

    void loadsTheDaemonValueAndSetsRoundTrip() {
        m_daemon.audioOutput = false;
        SettingsClient client(m_socketPath);
        QTRY_VERIFY(client.loaded());
        QVERIFY(!client.audioOutput());

        client.setAudioOutput(true);
        // Optimistic immediately, authoritative after the round trip.
        QVERIFY(client.audioOutput());
        QTRY_COMPARE(m_daemon.sets, 1);
        QTRY_VERIFY(!client.busy());
        QVERIFY(m_daemon.audioOutput);
        QVERIFY(client.errorMessage().isEmpty());
    }

    void daemonErrorSurfacesWithoutLosingState() {
        SettingsClient client(m_socketPath);
        QTRY_VERIFY(client.loaded());
        m_daemon.failNext = true;
        client.setAudioOutput(false);
        QTRY_VERIFY(!client.errorMessage().isEmpty());
        // A later successful refresh clears the error and restores the
        // daemon's authoritative value.
        client.refresh();
        QTRY_VERIFY(client.errorMessage().isEmpty());
        QTRY_VERIFY(client.audioOutput());
    }

private:
    QTemporaryDir m_root;
    StubSettingsDaemon m_daemon;
    QString m_socketPath;
};

QTEST_GUILESS_MAIN(SettingsClientTest)
#include "settingsclienttest.moc"
