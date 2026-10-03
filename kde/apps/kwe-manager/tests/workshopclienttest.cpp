// SPDX-License-Identifier: GPL-3.0-or-later
#include "../src/workshopclient.h"

#include "fakeworkshopdaemon.h"

#include <QTemporaryDir>
#include <QtTest>

using Answer = FakeWorkshopDaemon::Answer;

class WorkshopClientTest final : public QObject {
  Q_OBJECT

private slots:
  void initTestCase() {
    QVERIFY(m_root.isValid());
    m_path = m_root.path() + QStringLiteral("/daemon.sock");
    QVERIFY(m_daemon.listen(m_path));
  }
  void init() {
    m_daemon.methods.clear();
    m_daemon.paramsLog.clear();
    m_daemon.held.clear();
    m_daemon.handler = nullptr;
  }

  void rejectsMalformedIds() {
    WorkshopClient client;
    QVERIFY(!client.openItem(QStringLiteral("abc")));
    QVERIFY(!client.errorMessage().isEmpty());
    QVERIFY(!client.openItem(QStringLiteral("0")));
    QVERIFY(!client.openItem(QStringLiteral("123456789012345678901")));
  }

  void statusSignedOut() {
    m_daemon.handler = [](const QString &, const QJsonObject &) {
      return FakeWorkshopDaemon::okAnswer({{"worker_available", true}, {"signed_in", false}, {"account_name", ""}});
    };
    WorkshopClient client(m_path);
    QCOMPARE(client.authState(), WorkshopClient::Unknown);
    client.refreshAuthStatus();
    QTRY_COMPARE(client.authState(), WorkshopClient::SignedOut);
  }

  void statusSignedInAndWorkerMissing() {
    m_daemon.handler = [](const QString &, const QJsonObject &) {
      return FakeWorkshopDaemon::okAnswer({{"worker_available", true}, {"signed_in", true}, {"account_name", "fake_user"}});
    };
    WorkshopClient client(m_path);
    client.refreshAuthStatus();
    QTRY_COMPARE(client.authState(), WorkshopClient::SignedIn);
    QCOMPARE(client.accountName(), QStringLiteral("fake_user"));

    m_daemon.handler = [](const QString &, const QJsonObject &) {
      return FakeWorkshopDaemon::okAnswer({{"worker_available", false}, {"signed_in", false}, {"account_name", ""}});
    };
    client.refreshAuthStatus();
    QTRY_COMPARE(client.authState(), WorkshopClient::WorkerUnavailable);
  }

  void signInSequence() {
    int polls = 0;
    m_daemon.handler = [&polls](const QString &method, const QJsonObject &) {
      if (method == "workshop.auth.begin")
        return FakeWorkshopDaemon::okAnswer({{"challenge_url", "https://s.example/q/1"},
                                             {"poll_interval_ms", 1}, {"expires_in_ms", 100000}});
      ++polls;
      switch (polls) {
      case 1: return FakeWorkshopDaemon::okAnswer({{"state", "awaiting_scan"}, {"challenge_url", "https://s.example/q/1"}});
      case 2: return FakeWorkshopDaemon::okAnswer({{"state", "awaiting_confirmation"}});
      case 3: return FakeWorkshopDaemon::okAnswer({{"state", "challenge_changed"}, {"challenge_url", "https://s.example/q/2"}});
      default: return FakeWorkshopDaemon::okAnswer({{"state", "signed_in"}, {"account_name", "fake_user"}});
      }
    };
    WorkshopClient client(m_path);
    client.setPollIntervalBounds(5, 50);
    QList<int> states;
    connect(&client, &WorkshopClient::authStateChanged, this,
            [&] { states.push_back(client.authState()); });
    client.beginSignIn();
    QCOMPARE(client.authState(), WorkshopClient::Starting);
    QTRY_COMPARE(client.authState(), WorkshopClient::SignedIn);
    QCOMPARE(client.accountName(), QStringLiteral("fake_user"));
    QVERIFY(client.challengeUrl().isEmpty());
    QVERIFY(states.contains(WorkshopClient::AwaitingScan));
    QVERIFY(states.contains(WorkshopClient::AwaitingConfirmation));
    // Polling stops after signed_in.
    const int seen = m_daemon.count("workshop.auth.poll");
    QCOMPARE(seen, 4);
    QTest::qWait(100);
    QCOMPARE(m_daemon.count("workshop.auth.poll"), 4);
  }

  void challengeChangedUpdatesUrl() {
    int polls = 0;
    m_daemon.handler = [&polls](const QString &method, const QJsonObject &) {
      if (method == "workshop.auth.begin")
        return FakeWorkshopDaemon::okAnswer({{"challenge_url", "https://s.example/q/1"}, {"poll_interval_ms", 1}});
      if (++polls == 1)
        return FakeWorkshopDaemon::okAnswer({{"state", "challenge_changed"}, {"challenge_url", "https://s.example/q/2"}});
      return FakeWorkshopDaemon::okAnswer({{"state", "awaiting_scan"}});
    };
    WorkshopClient client(m_path);
    client.setPollIntervalBounds(5, 50);
    client.beginSignIn();
    QTRY_COMPARE(client.challengeUrl(), QStringLiteral("https://s.example/q/2"));
    QCOMPARE(client.authState(), WorkshopClient::AwaitingScan);
    client.cancelSignIn();
  }

  void expiredRestartsThenFails() {
    m_daemon.handler = [](const QString &method, const QJsonObject &) {
      if (method == "workshop.auth.begin")
        return FakeWorkshopDaemon::okAnswer({{"challenge_url", "https://s.example/q/x"}, {"poll_interval_ms", 1}});
      return FakeWorkshopDaemon::okAnswer({{"state", "expired"}});
    };
    WorkshopClient client(m_path);
    client.setPollIntervalBounds(5, 50);
    client.beginSignIn();
    QTRY_COMPARE(client.authState(), WorkshopClient::Failed);
    QVERIFY(!client.authError().isEmpty());
    // The first begin plus three automatic restarts.
    QCOMPARE(m_daemon.count("workshop.auth.begin"), 4);
    // A new user attempt starts over.
    client.beginSignIn();
    QCOMPARE(client.authState(), WorkshopClient::Starting);
    QTRY_COMPARE(client.authState(), WorkshopClient::Failed);
    QCOMPARE(m_daemon.count("workshop.auth.begin"), 8);
  }

  void cancelStopsPollingAndNotifiesDaemon() {
    m_daemon.handler = [](const QString &method, const QJsonObject &) {
      if (method == "workshop.auth.begin")
        return FakeWorkshopDaemon::okAnswer({{"challenge_url", "https://s.example/q/1"}, {"poll_interval_ms", 1}});
      if (method == "workshop.auth.poll")
        return FakeWorkshopDaemon::okAnswer({{"state", "awaiting_scan"}});
      return FakeWorkshopDaemon::okAnswer();
    };
    WorkshopClient client(m_path);
    client.setPollIntervalBounds(5, 50);
    client.beginSignIn();
    QTRY_VERIFY(m_daemon.count("workshop.auth.poll") >= 2);
    client.cancelSignIn();
    QCOMPARE(client.authState(), WorkshopClient::SignedOut);
    QTRY_COMPARE(m_daemon.count("workshop.auth.cancel"), 1);
    QTest::qWait(50);  // let an in-flight poll land
    const int polls = m_daemon.count("workshop.auth.poll");
    QTest::qWait(150);
    QCOMPARE(m_daemon.count("workshop.auth.poll"), polls);
    QCOMPARE(client.authState(), WorkshopClient::SignedOut);
  }

  void busyPollIsTolerated() {
    int polls = 0;
    m_daemon.handler = [&polls](const QString &method, const QJsonObject &) {
      if (method == "workshop.auth.begin")
        return FakeWorkshopDaemon::okAnswer({{"challenge_url", "https://s.example/q/1"}, {"poll_interval_ms", 1}});
      if (++polls < 3)
        return FakeWorkshopDaemon::errorAnswer(QStringLiteral("busy"));
      return FakeWorkshopDaemon::okAnswer({{"state", "signed_in"}, {"account_name", "fake_user"}});
    };
    WorkshopClient client(m_path);
    client.setPollIntervalBounds(5, 50);
    client.beginSignIn();
    QTRY_COMPARE(client.authState(), WorkshopClient::SignedIn);
    QVERIFY(client.authError().isEmpty());
  }

  void errorMappings() {
    m_daemon.handler = [](const QString &, const QJsonObject &) {
      return FakeWorkshopDaemon::errorAnswer(QStringLiteral("worker_unavailable"));
    };
    WorkshopClient client(m_path);
    client.beginSignIn();
    QTRY_COMPARE(client.authState(), WorkshopClient::WorkerUnavailable);
    QVERIFY(!client.authError().isEmpty());

    m_daemon.handler = [](const QString &, const QJsonObject &) {
      return FakeWorkshopDaemon::errorAnswer(QStringLiteral("offline"));
    };
    client.beginSignIn();
    QTRY_COMPARE(client.authState(), WorkshopClient::Failed);
    QCOMPARE(client.authError(), QStringLiteral("Steam is unreachable. Check your network connection."));

    QCOMPARE(WorkshopClient::describeError("rate_limited", ""),
             QStringLiteral("Steam is limiting requests. Try again in a minute."));
    const QString capped = WorkshopClient::describeError("steam_error", QString(5000, QLatin1Char('x')));
    QVERIFY(capped.size() < 300);
  }

  void signedOutReplyFlipsState() {
    m_daemon.handler = [](const QString &method, const QJsonObject &) {
      if (method == "workshop.auth.status")
        return FakeWorkshopDaemon::okAnswer({{"worker_available", true}, {"signed_in", true}, {"account_name", "fake_user"}});
      return FakeWorkshopDaemon::errorAnswer(QStringLiteral("signed_out"));
    };
    WorkshopClient client(m_path);
    client.refreshAuthStatus();
    QTRY_COMPARE(client.authState(), WorkshopClient::SignedIn);
    client.call("workshop.query", {}, this, [](const WorkshopClient::Reply &) {});
    QTRY_COMPARE(client.authState(), WorkshopClient::SignedOut);
    QVERIFY(client.accountName().isEmpty());
  }

  void signOutClearsAccount() {
    m_daemon.handler = [](const QString &method, const QJsonObject &) {
      if (method == "workshop.auth.status")
        return FakeWorkshopDaemon::okAnswer({{"worker_available", true}, {"signed_in", true}, {"account_name", "fake_user"}});
      return FakeWorkshopDaemon::okAnswer({{"signed_in", false}});
    };
    WorkshopClient client(m_path);
    client.refreshAuthStatus();
    QTRY_COMPARE(client.authState(), WorkshopClient::SignedIn);
    client.signOut();
    QTRY_COMPARE(client.authState(), WorkshopClient::SignedOut);
    QVERIFY(client.accountName().isEmpty());
  }

  void requestDeadlineReportsTimeout() {
    m_daemon.handler = [](const QString &, const QJsonObject &) { return FakeWorkshopDaemon::holdAnswer(); };
    WorkshopClient client(m_path);
    client.setRequestDeadline(50);
    QString error;
    client.call("workshop.query", {}, this, [&](const WorkshopClient::Reply &r) { error = r.error; });
    QTRY_COMPARE(error, QStringLiteral("timeout"));
  }

private:
  QTemporaryDir m_root;
  FakeWorkshopDaemon m_daemon;
  QString m_path;
};

QTEST_GUILESS_MAIN(WorkshopClientTest)
#include "workshopclienttest.moc"
