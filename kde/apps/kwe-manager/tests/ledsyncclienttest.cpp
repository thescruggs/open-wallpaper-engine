// SPDX-License-Identifier: GPL-3.0-or-later
#include "../src/ledsyncclient.h"

#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLocalServer>
#include <QLocalSocket>
#include <QSignalSpy>
#include <QTemporaryDir>
#include <QtTest>

// Daemon stand-in for led.status / led.reprobe / settings.set {led}, shaped
// after docs/SUPERVISOR_API_V1.md "LED sync".
class FakeLedDaemon final : public QObject {
    Q_OBJECT
public:
    FakeLedDaemon() {
        connect(&m_server, &QLocalServer::newConnection, this, &FakeLedDaemon::onConnection);
        reset();
    }
    bool listen(const QString &path) { return m_server.listen(path); }
    void close() { m_server.close(); }

    void reset() {
        legacy = false;
        noSupervisor = false;
        led = QJsonObject{{QStringLiteral("enabled"), true},
                          {QStringLiteral("mode"), QStringLiteral("ambient")},
                          {QStringLiteral("brightness"), 100},
                          {QStringLiteral("rate_hz"), 10},
                          {QStringLiteral("disabled_devices"), QJsonArray{}}};
        state = QStringLiteral("server_absent");
        reason = QJsonValue::Null;
        paused = false;
        workerStatus = QJsonValue::Null;
        reprobeReply = {};
        sets.clear();
        statusCalls = 0;
        reprobeCalls = 0;
    }

    bool legacy = false;
    bool noSupervisor = false;
    QJsonObject led;
    QString state;
    QJsonValue reason;
    bool paused = false;
    QJsonValue workerStatus;
    // Overrides {accepted, action, reason, retry_after_ms}; empty = accepted.
    QJsonObject reprobeReply;
    QList<QJsonObject> sets;
    int statusCalls = 0;
    int reprobeCalls = 0;

    QJsonObject status() const {
        return QJsonObject{{QStringLiteral("enabled"), led.value(QStringLiteral("enabled"))},
                           {QStringLiteral("state"), state},
                           {QStringLiteral("reason"), reason},
                           {QStringLiteral("paused"), paused},
                           {QStringLiteral("settings"), led},
                           {QStringLiteral("worker"), QJsonObject{}},
                           {QStringLiteral("worker_status"), workerStatus},
                           {QStringLiteral("tee"), QJsonObject{}}};
    }

private:
    QLocalServer m_server;

    // Daemon-side bounds; an out-of-bounds value rejects the whole request.
    static QString validate(const QJsonObject &patch) {
        static const QStringList fields{QStringLiteral("enabled"), QStringLiteral("mode"),
                                        QStringLiteral("brightness"), QStringLiteral("rate_hz"),
                                        QStringLiteral("disabled_devices")};
        if (patch.isEmpty())
            return QStringLiteral("settings.set needs at least one of ...");
        for (auto it = patch.begin(); it != patch.end(); ++it) {
            if (!fields.contains(it.key()))
                return QStringLiteral("unknown field `%1`").arg(it.key());
        }
        if (patch.contains(QStringLiteral("brightness")) &&
            patch.value(QStringLiteral("brightness")).toInt(-1) > 100)
            return QStringLiteral("led.brightness must be 0..=100");
        if (patch.contains(QStringLiteral("rate_hz"))) {
            const int rate = patch.value(QStringLiteral("rate_hz")).toInt(0);
            if (rate < 1 || rate > 20)
                return QStringLiteral("led.rate_hz must be 1..=20");
        }
        if (patch.value(QStringLiteral("disabled_devices")).toArray().size() > 128)
            return QStringLiteral("led.disabled_devices holds at most 128 ids");
        return {};
    }

    void onConnection() {
        auto *socket = m_server.nextPendingConnection();
        connect(socket, &QLocalSocket::readyRead, this, [this, socket] {
            const auto newline = socket->peek(64 * 1024).indexOf('\n');
            if (newline < 0)
                return;
            const auto request = QJsonDocument::fromJson(socket->read(newline + 1)).object();
            const auto method = request.value(QStringLiteral("method")).toString();
            const auto params = request.value(QStringLiteral("params")).toObject();
            QJsonObject result;
            bool ok = true;
            auto fail = [&](const QString &error, const QString &detail = {}) {
                ok = false;
                result = QJsonObject{{QStringLiteral("error"), error}};
                if (!detail.isEmpty())
                    result.insert(QStringLiteral("detail"), detail);
            };
            if (method == QStringLiteral("led.status") && !legacy) {
                ++statusCalls;
                if (noSupervisor)
                    fail(QStringLiteral("led_unavailable"));
                else
                    result = status();
            } else if (method == QStringLiteral("led.reprobe") && !legacy) {
                ++reprobeCalls;
                result = QJsonObject{{QStringLiteral("accepted"), true},
                                     {QStringLiteral("action"), QStringLiteral("reprobe")},
                                     {QStringLiteral("reason"), QJsonValue::Null},
                                     {QStringLiteral("retry_after_ms"), QJsonValue::Null}};
                for (auto it = reprobeReply.begin(); it != reprobeReply.end(); ++it)
                    result.insert(it.key(), it.value());
                result.insert(QStringLiteral("status"), status());
            } else if (method == QStringLiteral("settings.set")) {
                sets.push_back(params);
                if (legacy) {
                    fail(QStringLiteral("invalid_params"), QStringLiteral("unknown field `led`"));
                } else {
                    const auto patch = params.value(QStringLiteral("led")).toObject();
                    const auto error = validate(patch);
                    if (!error.isEmpty()) {
                        fail(QStringLiteral("invalid_params"), error);
                    } else {
                        for (auto it = patch.begin(); it != patch.end(); ++it)
                            led.insert(it.key(), it.value());
                        if (patch.contains(QStringLiteral("enabled")))
                            state = led.value(QStringLiteral("enabled")).toBool()
                                        ? QStringLiteral("starting")
                                        : QStringLiteral("disabled");
                        result = QJsonObject{{QStringLiteral("audio_output"), true},
                                             {QStringLiteral("pause_when_covered"), false},
                                             {QStringLiteral("pause_when_display_off"), false},
                                             {QStringLiteral("led"), led}};
                    }
                }
            } else {
                fail(QStringLiteral("unknown_method"));
            }
            socket->write(QJsonDocument(QJsonObject{
                              {QStringLiteral("version"), 1},
                              {QStringLiteral("id"), request.value(QStringLiteral("id"))},
                              {QStringLiteral("ok"), ok},
                              {QStringLiteral("result"), result},
                          }).toJson(QJsonDocument::Compact) + '\n');
            socket->flush();
        });
    }
};

namespace {
QJsonObject device(const QString &id, const QString &name, const QString &vendor, int leds) {
    return QJsonObject{{QStringLiteral("id"), id},
                       {QStringLiteral("name"), name},
                       {QStringLiteral("vendor"), vendor},
                       {QStringLiteral("serial"), QString()},
                       {QStringLiteral("location"), QString()},
                       {QStringLiteral("type"), 5},
                       {QStringLiteral("leds"), leds},
                       {QStringLiteral("enabled"), true},
                       {QStringLiteral("controlled"), true},
                       {QStringLiteral("zones"), QJsonArray{}},
                       {QStringLiteral("zones_truncated"), false}};
}

QJsonObject workerStatus(const QString &state, const QJsonArray &devices, bool truncated,
                         int deviceCount, bool audioFresh) {
    return QJsonObject{
        {QStringLiteral("version"), 1},
        {QStringLiteral("type"), QStringLiteral("led_status")},
        {QStringLiteral("seq"), 3},
        {QStringLiteral("state"), state},
        {QStringLiteral("reason"), QJsonValue::Null},
        {QStringLiteral("server"),
         QJsonObject{{QStringLiteral("port"), 6742},
                     {QStringLiteral("reachable"), state == QStringLiteral("active")},
                     {QStringLiteral("protocol"), 4},
                     {QStringLiteral("server_protocol"), 5}}},
        {QStringLiteral("audio"),
         QJsonObject{{QStringLiteral("fresh"), audioFresh},
                     {QStringLiteral("bands"), audioFresh ? QJsonValue(32) : QJsonValue()}}},
        {QStringLiteral("device_count"), deviceCount},
        {QStringLiteral("devices"), devices},
        {QStringLiteral("devices_truncated"), truncated},
    };
}

const QString KeyboardId = QStringLiteral("9d0c2b7e5a1f3c44");
const QString StripId = QStringLiteral("0123456789abcdef");
} // namespace

class LedSyncClientTest final : public QObject {
    Q_OBJECT

private slots:
    void initTestCase() {
        QVERIFY(m_root.isValid());
        m_socketPath = m_root.path() + QStringLiteral("/daemon.sock");
        QVERIFY(m_daemon.listen(m_socketPath));
    }

    void init() { m_daemon.reset(); }

    void startsLoadingAndReadsServerAbsent() {
        LedSyncClient client(m_socketPath);
        QCOMPARE(client.support(), QStringLiteral("loading"));
        // No request until the page asks (polling is page-driven).
        QTest::qWait(50);
        QCOMPARE(m_daemon.statusCalls, 0);
        client.refresh();
        QTRY_COMPARE(client.support(), QStringLiteral("ready"));
        QCOMPARE(client.state(), QStringLiteral("server_absent"));
        QCOMPARE(client.reason(), QString());
        QVERIFY(!client.hasWorkerStatus());
        QVERIFY(client.devices().isEmpty());
        QVERIFY(client.enabled());
        QCOMPARE(client.mode(), QStringLiteral("ambient"));
        QCOMPARE(client.brightness(), 100);
        QCOMPARE(client.rateHz(), 10);
        QVERIFY(client.errorMessage().isEmpty());
    }

    void parsesEveryDaemonState_data() {
        QTest::addColumn<QString>("state");
        QTest::addColumn<QString>("reason");
        QTest::addColumn<bool>("paused");
        QTest::newRow("disabled") << "disabled" << QString() << false;
        QTest::newRow("not_installed") << "worker_unavailable" << "not_installed" << false;
        QTest::newRow("not_configured") << "worker_unavailable" << "not_configured" << false;
        QTest::newRow("budget") << "worker_unavailable" << "restart_budget_exhausted" << false;
        QTest::newRow("starting") << "starting" << QString() << false;
        QTest::newRow("restarting") << "starting" << "restarting" << false;
        QTest::newRow("worker_stopping") << "starting" << "worker_stopping" << false;
        QTest::newRow("connecting") << "connecting" << QString() << false;
        QTest::newRow("server_absent") << "server_absent" << QString() << false;
        QTest::newRow("active") << "active" << QString() << false;
        QTest::newRow("off_paused") << "off" << "paused" << true;
        QTest::newRow("off_user") << "off" << QString() << false;
        QTest::newRow("degraded") << "degraded" << "timeout" << false;
        QTest::newRow("unresponsive") << "degraded" << "worker_unresponsive" << false;
    }

    void parsesEveryDaemonState() {
        QFETCH(QString, state);
        QFETCH(QString, reason);
        QFETCH(bool, paused);
        m_daemon.state = state;
        m_daemon.reason = reason.isEmpty() ? QJsonValue(QJsonValue::Null) : QJsonValue(reason);
        m_daemon.paused = paused;
        LedSyncClient client(m_socketPath);
        client.refresh();
        QTRY_COMPARE(client.support(), QStringLiteral("ready"));
        QCOMPARE(client.state(), state);
        QCOMPARE(client.reason(), reason);
        QCOMPARE(client.paused(), paused);
    }

    void activeWithTwoDevices() {
        m_daemon.state = QStringLiteral("active");
        m_daemon.led.insert(QStringLiteral("disabled_devices"), QJsonArray{StripId});
        m_daemon.workerStatus = workerStatus(
            QStringLiteral("active"),
            QJsonArray{device(KeyboardId, QStringLiteral("Keyboard"), QStringLiteral("Vendor A"), 104),
                       device(StripId, QStringLiteral("Strip"), QString(), 30)},
            false, 2, false);
        LedSyncClient client(m_socketPath);
        client.refresh();
        QTRY_COMPARE(client.state(), QStringLiteral("active"));
        QVERIFY(client.hasWorkerStatus());
        QCOMPARE(client.protocolVersion(), 4);
        QCOMPARE(client.sdkPort(), 6742);
        QCOMPARE(client.deviceCount(), 2);
        QVERIFY(!client.devicesTruncated());
        QVERIFY(!client.audioFresh());
        const auto devices = client.devices();
        QCOMPARE(devices.size(), 2);
        const auto first = devices.at(0).toMap();
        QCOMPARE(first.value(QStringLiteral("id")).toString(), KeyboardId);
        QCOMPARE(first.value(QStringLiteral("name")).toString(), QStringLiteral("Keyboard"));
        QCOMPARE(first.value(QStringLiteral("vendor")).toString(), QStringLiteral("Vendor A"));
        QCOMPARE(first.value(QStringLiteral("leds")).toInt(), 104);
        QCOMPARE(devices.at(1).toMap().value(QStringLiteral("vendor")).toString(), QString());
        QVERIFY(client.isDeviceEnabled(KeyboardId));
        QVERIFY(!client.isDeviceEnabled(StripId));
    }

    void truncatedDeviceListIsFlagged() {
        m_daemon.state = QStringLiteral("active");
        QJsonArray devices;
        for (int i = 0; i < 32; ++i)
            devices.push_back(device(QStringLiteral("%1").arg(i, 16, 16, QLatin1Char('0')),
                                     QStringLiteral("Device %1").arg(i), QString(), 1));
        m_daemon.workerStatus = workerStatus(QStringLiteral("active"), devices, true, 40, true);
        LedSyncClient client(m_socketPath);
        client.refresh();
        QTRY_COMPARE(client.devices().size(), 32);
        QVERIFY(client.devicesTruncated());
        QCOMPARE(client.deviceCount(), 40);
        QVERIFY(client.audioFresh());
    }

    void writesOnlyTheChangedFields() {
        LedSyncClient client(m_socketPath);
        client.refresh();
        QTRY_COMPARE(client.support(), QStringLiteral("ready"));

        client.setMode(QStringLiteral("spectrum"));
        // Optimistic, then authoritative.
        QCOMPARE(client.mode(), QStringLiteral("spectrum"));
        QTRY_COMPARE(m_daemon.sets.size(), 1);
        QCOMPARE(m_daemon.sets.at(0),
                 (QJsonObject{{QStringLiteral("led"),
                               QJsonObject{{QStringLiteral("mode"), QStringLiteral("spectrum")}}}}));
        QTRY_VERIFY(!client.busy());
        QCOMPARE(client.mode(), QStringLiteral("spectrum"));
        QCOMPARE(m_daemon.led.value(QStringLiteral("brightness")).toInt(), 100);

        client.setEnabled(false);
        QTRY_COMPARE(m_daemon.sets.size(), 2);
        QCOMPARE(m_daemon.sets.at(1).value(QStringLiteral("led")).toObject(),
                 (QJsonObject{{QStringLiteral("enabled"), false}}));
        QTRY_COMPARE(client.state(), QStringLiteral("disabled"));
        QVERIFY(!client.enabled());
        QVERIFY(client.errorMessage().isEmpty());

        client.setMode(QStringLiteral("rainbow")); // not a mode: ignored
        QTest::qWait(LedSyncClient::DebounceMilliseconds + 100);
        QCOMPARE(m_daemon.sets.size(), 2);
    }

    void sliderDragIsDebouncedIntoOneWrite() {
        LedSyncClient client(m_socketPath);
        client.refresh();
        QTRY_COMPARE(client.support(), QStringLiteral("ready"));
        for (int value = 100; value >= 40; value -= 5) {
            client.setBrightness(value);
            QTest::qWait(10);
        }
        client.setRateHz(15);
        QCOMPARE(client.brightness(), 40);
        QCOMPARE(m_daemon.sets.size(), 0);
        QTRY_COMPARE(m_daemon.sets.size(), 1);
        QCOMPARE(m_daemon.sets.at(0).value(QStringLiteral("led")).toObject(),
                 (QJsonObject{{QStringLiteral("brightness"), 40}, {QStringLiteral("rate_hz"), 15}}));
        QTRY_VERIFY(!client.busy());
        QCOMPARE(client.brightness(), 40);
        QCOMPARE(client.rateHz(), 15);
        // Clamped to the documented bounds before sending.
        client.setRateHz(99);
        client.flush();
        QTRY_COMPARE(m_daemon.sets.size(), 2);
        QCOMPARE(m_daemon.sets.at(1).value(QStringLiteral("led")).toObject(),
                 (QJsonObject{{QStringLiteral("rate_hz"), 20}}));
    }

    void deviceToggleReplacesTheWholeListKeepingUnknownIds() {
        const QString absentId = QStringLiteral("ffffffffffffffff");
        m_daemon.state = QStringLiteral("active");
        m_daemon.led.insert(QStringLiteral("disabled_devices"), QJsonArray{absentId});
        m_daemon.workerStatus = workerStatus(
            QStringLiteral("active"),
            QJsonArray{device(KeyboardId, QStringLiteral("Keyboard"), QString(), 104)}, false, 1, false);
        LedSyncClient client(m_socketPath);
        client.refresh();
        QTRY_COMPARE(client.devices().size(), 1);

        client.setDeviceEnabled(KeyboardId, false);
        QTRY_COMPARE(m_daemon.sets.size(), 1);
        QCOMPARE(m_daemon.sets.at(0).value(QStringLiteral("led")).toObject(),
                 (QJsonObject{{QStringLiteral("disabled_devices"), QJsonArray{KeyboardId, absentId}}}));
        QTRY_VERIFY(!client.busy());
        QVERIFY(!client.isDeviceEnabled(KeyboardId));

        client.setDeviceEnabled(KeyboardId, true);
        QTRY_COMPARE(m_daemon.sets.size(), 2);
        QCOMPARE(m_daemon.sets.at(1).value(QStringLiteral("led")).toObject(),
                 (QJsonObject{{QStringLiteral("disabled_devices"), QJsonArray{absentId}}}));

        client.setDeviceEnabled(QStringLiteral("NOT-AN-ID"), false); // ignored
        QTest::qWait(LedSyncClient::DebounceMilliseconds + 100);
        QCOMPARE(m_daemon.sets.size(), 2);
    }

    void rejectedValueIsSurfacedAndRevertsToTheDaemonRecord() {
        m_daemon.led.insert(QStringLiteral("disabled_devices"), [] {
            QJsonArray ids;
            for (int i = 0; i < 128; ++i)
                ids.push_back(QStringLiteral("%1").arg(i + 1, 16, 16, QLatin1Char('0')));
            return ids;
        }());
        LedSyncClient client(m_socketPath);
        client.refresh();
        QTRY_COMPARE(client.support(), QStringLiteral("ready"));
        QCOMPARE(client.disabledDevices().size(), 128);

        // The 129th id exceeds the daemon's bound: the whole patch is refused.
        client.setDeviceEnabled(KeyboardId, false);
        QCOMPARE(client.disabledDevices().size(), 129);
        QTRY_VERIFY(!client.errorMessage().isEmpty());
        QVERIFY(client.errorMessage().contains(QStringLiteral("at most 128")));
        QTRY_COMPARE(client.disabledDevices().size(), 128);
        QVERIFY(client.isDeviceEnabled(KeyboardId));
        QCOMPARE(m_daemon.led.value(QStringLiteral("disabled_devices")).toArray().size(), 128);
        QCOMPARE(client.support(), QStringLiteral("ready"));
    }

    void reprobeAcceptedThenRateLimited() {
        LedSyncClient client(m_socketPath);
        client.refresh();
        QTRY_COMPARE(client.support(), QStringLiteral("ready"));
        QVERIFY(client.canReprobe());

        client.reprobe();
        QVERIFY(!client.canReprobe());
        QTRY_COMPARE(m_daemon.reprobeCalls, 1);
        // Our own 2 s cooldown matches the daemon's limit: a second click
        // during it sends nothing.
        QTest::qWait(100);
        QVERIFY(!client.canReprobe());
        client.reprobe();
        QTest::qWait(100);
        QCOMPARE(m_daemon.reprobeCalls, 1);
        // The follow-up status poll picks up the new state.
        const int before = m_daemon.statusCalls;
        QTRY_VERIFY(m_daemon.statusCalls > before);
        QTRY_VERIFY_WITH_TIMEOUT(client.canReprobe(), LedSyncClient::ReprobeCooldownMilliseconds + 1000);

        // The daemon says rate_limited (e.g. another manager clicked): the
        // button is disabled for retry_after_ms, no error is shown.
        m_daemon.reprobeReply = QJsonObject{{QStringLiteral("accepted"), false},
                                            {QStringLiteral("action"), QJsonValue::Null},
                                            {QStringLiteral("reason"), QStringLiteral("rate_limited")},
                                            {QStringLiteral("retry_after_ms"), 600}};
        client.reprobe();
        QTRY_COMPARE(m_daemon.reprobeCalls, 2);
        QTest::qWait(150);
        QVERIFY(!client.canReprobe());
        QVERIFY(client.errorMessage().isEmpty());
        QTRY_VERIFY_WITH_TIMEOUT(client.canReprobe(), 1500);
    }

    void olderDaemonWithoutLedSupport() {
        m_daemon.legacy = true;
        LedSyncClient client(m_socketPath);
        client.refresh();
        QTRY_COMPARE(client.support(), QStringLiteral("unsupported"));
        QVERIFY(client.errorMessage().isEmpty());
        client.reprobe();
        QTRY_VERIFY(client.canReprobe());
        QCOMPARE(client.support(), QStringLiteral("unsupported"));
    }

    void daemonWithoutSupervisor() {
        m_daemon.noSupervisor = true;
        LedSyncClient client(m_socketPath);
        client.refresh();
        QTRY_COMPARE(client.support(), QStringLiteral("unavailable"));
    }

    void unreachableDaemonThenRecovery() {
        QTemporaryDir dir;
        const QString path = dir.path() + QStringLiteral("/absent.sock");
        LedSyncClient client(path);
        client.refresh();
        QTRY_COMPARE(client.support(), QStringLiteral("unreachable"));
        QVERIFY(!client.errorMessage().isEmpty());
        // An edit while unreachable is not applied: the control returns to
        // the last confirmed value and the error stays visible.
        client.setEnabled(false);
        QTRY_VERIFY(client.enabled());
        QVERIFY(!client.busy());

        FakeLedDaemon late;
        QVERIFY(late.listen(path));
        client.refresh();
        QTRY_COMPARE(client.support(), QStringLiteral("ready"));
        QVERIFY(client.errorMessage().isEmpty());
        QCOMPARE(client.state(), QStringLiteral("server_absent"));
    }

private:
    QTemporaryDir m_root;
    FakeLedDaemon m_daemon;
    QString m_socketPath;
};

QTEST_GUILESS_MAIN(LedSyncClientTest)
#include "ledsyncclienttest.moc"
