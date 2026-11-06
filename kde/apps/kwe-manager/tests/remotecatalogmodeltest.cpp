// SPDX-License-Identifier: GPL-3.0-or-later
#include "../src/catalogmodel.h"
#include "../src/remotecatalogmodel.h"
#include "../src/workshopclient.h"

#include "fakeworkshopdaemon.h"

#include <QJsonArray>
#include <QTemporaryDir>
#include <QtTest>

using Answer = FakeWorkshopDaemon::Answer;
using Fake = FakeWorkshopDaemon;

namespace {
QJsonObject item(const QString &id, const QString &preview = {}, const QString &file = {}) {
  return QJsonObject{{"id", id}, {"title", "Fake item " + id}, {"short_description", "fake"},
                     {"preview_url", preview}, {"preview_file", file},
                     {"tags", QJsonArray{"Scene"}}, {"kind", "scene"}, {"creator_id", "1"},
                     {"file_size", 1000}, {"subscriptions", 5}, {"score", 0.5},
                     {"time_updated", 1}};
}
Answer page(const QStringList &ids, bool hasMore, int pageNo = 1) {
  QJsonArray items;
  for (const auto &id : ids)
    items.push_back(item(id));
  return Fake::okAnswer({{"items", items}, {"page", pageNo}, {"total", 99}, {"has_more", hasMore}});
}
}

class RemoteCatalogModelTest final : public QObject {
  Q_OBJECT

  QTemporaryDir m_root;
  Fake m_daemon;
  QString m_path;
  std::unique_ptr<WorkshopClient> m_client;
  std::unique_ptr<CatalogModel> m_local;
  std::unique_ptr<RemoteCatalogModel> m_model;

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
    m_client = std::make_unique<WorkshopClient>(m_path);
    m_local = std::make_unique<CatalogModel>();
    m_model = std::make_unique<RemoteCatalogModel>(m_client.get(), m_local.get());
  }
  void cleanup() {
    m_model.reset();
    m_local.reset();
    m_client.reset();
  }

  void refreshFillsRowsAndIsReady() {
    m_daemon.handler = [](const QString &, const QJsonObject &) { return page({"101", "102"}, true); };
    m_model->refresh();
    QCOMPARE(m_model->state(), RemoteCatalogModel::Loading);
    QTRY_COMPARE(m_model->state(), RemoteCatalogModel::Ready);
    QCOMPARE(m_model->rowCount(), 2);
    QVERIFY(m_model->hasMore());
    QCOMPARE(m_model->data(m_model->index(0), RemoteCatalogModel::WorkshopIdRole).toString(), QStringLiteral("101"));
    QVERIFY(m_model->data(m_model->index(0), RemoteCatalogModel::PreviewUrlRole).toUrl().isEmpty());
    QCOMPARE(m_daemon.paramsLog.first().value("page").toInt(), 1);
    QCOMPARE(m_daemon.paramsLog.first().value("sort").toString(), QStringLiteral("trend_week"));
  }

  void emptyResult() {
    m_daemon.handler = [](const QString &, const QJsonObject &) { return page({}, false); };
    m_model->refresh();
    QTRY_COMPARE(m_model->state(), RemoteCatalogModel::Empty);
    QCOMPARE(m_model->rowCount(), 0);
  }

  void loadMoreAppendsDedupesAndStops() {
    m_daemon.handler = [](const QString &method, const QJsonObject &params) {
      if (method != "workshop.query")
        return Fake::okAnswer({{"files", QJsonObject{}}});
      if (params.value("page").toInt() == 1)
        return page({"101", "102"}, true, 1);
      return page({"102", "103"}, false, 2);  // 102 repeats
    };
    m_model->refresh();
    QTRY_COMPARE(m_model->state(), RemoteCatalogModel::Ready);
    m_model->loadMore();
    QVERIFY(m_model->loadingMore());
    m_model->loadMore();  // no-op while in flight
    QTRY_VERIFY(!m_model->loadingMore());
    QCOMPARE(m_model->rowCount(), 3);
    QVERIFY(!m_model->hasMore());
    const int queries = m_daemon.count("workshop.query");
    QCOMPARE(queries, 2);
    m_model->loadMore();  // !hasMore: no-op
    QTest::qWait(50);
    QCOMPARE(m_daemon.count("workshop.query"), 2);
  }

  void supersededSearchAnswerIsIgnored() {
    m_daemon.handler = [](const QString &method, const QJsonObject &params) {
      if (method != "workshop.query")
        return Fake::okAnswer({{"files", QJsonObject{}}});
      if (params.value("search").toString() == "old")
        return Fake::holdAnswer();
      return page({"201"}, false);
    };
    m_model->setSearchText("old");
    m_model->refresh();
    QTRY_COMPARE(m_daemon.held.size(), 1);
    m_model->setSearchText("new");
    m_model->refresh();
    QTRY_COMPARE(m_model->state(), RemoteCatalogModel::Ready);
    QCOMPARE(m_model->rowCount(), 1);
    m_daemon.release(0, page({"900", "901"}, true));
    QTest::qWait(100);
    QCOMPARE(m_model->rowCount(), 1);
    QCOMPARE(m_model->data(m_model->index(0), RemoteCatalogModel::WorkshopIdRole).toString(), QStringLiteral("201"));
    QVERIFY(!m_model->hasMore());
  }

  void errorCodesMapToStates() {
    const QList<QPair<QString, RemoteCatalogModel::State>> cases{
        {"offline", RemoteCatalogModel::Offline},
        {"signed_out", RemoteCatalogModel::SignedOut},
        {"rate_limited", RemoteCatalogModel::RateLimited},
        {"steam_error", RemoteCatalogModel::Failed}};
    for (const auto &c : cases) {
      m_daemon.handler = [code = c.first](const QString &, const QJsonObject &) {
        return Fake::errorAnswer(code, "fake detail");
      };
      m_model->refresh();
      QTRY_COMPARE(m_model->state(), c.second);
      QVERIFY(!m_model->errorMessage().isEmpty());
    }
    QCOMPARE(m_client->authState(), WorkshopClient::SignedOut);
  }

  void previewsUpdateTheRightRows() {
    m_daemon.handler = [](const QString &method, const QJsonObject &params) {
      if (method == "workshop.query") {
        QJsonArray items;
        items.push_back(item("301", "https://img.example/301.jpg"));
        items.push_back(item("302", "https://img.example/302.jpg"));
        items.push_back(item("303", "https://img.example/303.jpg", "/tmp/fake/303.jpg"));
        return Fake::okAnswer({{"items", items}, {"page", 1}, {"total", 3}, {"has_more", false}});
      }
      // Only 302 is cached now.
      Q_UNUSED(params)
      return Fake::okAnswer({{"files", QJsonObject{{"302", "/tmp/fake/302.jpg"}}}});
    };
    m_model->refresh();
    QTRY_COMPARE(m_model->state(), RemoteCatalogModel::Ready);
    // 301 never gets a file: it is asked for again, a bounded number of times.
    QTRY_COMPARE(m_daemon.count("workshop.previews"), 3);
    QTest::qWait(50);
    QCOMPARE(m_daemon.count("workshop.previews"), 3);
    const auto items = m_daemon.paramsLog.last().value("items").toArray();
    QCOMPARE(items.size(), 1);  // 302 arrived and 303 already had a file
    QCOMPARE(items.first().toObject().value("id").toString(), QStringLiteral("301"));
    QTRY_COMPARE(m_model->data(m_model->index(1), RemoteCatalogModel::PreviewUrlRole).toUrl(),
                 QUrl::fromLocalFile("/tmp/fake/302.jpg"));
    QVERIFY(m_model->data(m_model->index(0), RemoteCatalogModel::PreviewUrlRole).toUrl().isEmpty());
    QCOMPARE(m_model->data(m_model->index(2), RemoteCatalogModel::PreviewUrlRole).toUrl(),
             QUrl::fromLocalFile("/tmp/fake/303.jpg"));
  }

  void previewFailureIsSilent() {
    m_daemon.handler = [](const QString &method, const QJsonObject &) {
      if (method == "workshop.query")
        return Fake::okAnswer({{"items", QJsonArray{item("401", "https://img.example/401.jpg")}},
                               {"page", 1}, {"total", 1}, {"has_more", false}});
      return Fake::errorAnswer("offline");
    };
    m_model->refresh();
    QTRY_COMPARE(m_daemon.count("workshop.previews"), 3);
    QTest::qWait(50);
    QCOMPARE(m_daemon.count("workshop.previews"), 3);  // retries are bounded
    QCOMPARE(m_model->state(), RemoteCatalogModel::Ready);
    QVERIFY(m_model->errorMessage().isEmpty());
  }

  void subscribeLifecycle() {
    bool fail = false;
    m_daemon.handler = [&fail](const QString &method, const QJsonObject &) {
      if (method == "workshop.query")
        return page({"501"}, false);
      if (fail)
        return Fake::errorAnswer("rate_limited");
      return Fake::okAnswer({{"accepted", true}});
    };
    m_model->refresh();
    QTRY_COMPARE(m_model->state(), RemoteCatalogModel::Ready);
    const auto idx = m_model->index(0);
    QVERIFY(!m_model->data(idx, RemoteCatalogModel::SubscribedRole).toBool());

    m_model->subscribe("501");
    QVERIFY(m_model->data(idx, RemoteCatalogModel::SubscriptionPendingRole).toBool());
    QTRY_COMPARE(m_daemon.count("workshop.subscribe"), 1);
    QTest::qWait(50);
    // Accepted, but the local catalog does not list it yet: still pending.
    QVERIFY(m_model->data(idx, RemoteCatalogModel::SubscriptionPendingRole).toBool());

    // The local catalog reports the item: pending clears, subscribed is true.
    m_local->replaceFromCatalog(QJsonObject{{"items", QJsonArray{QJsonObject{
        {"workshop_id", "501"}, {"title", "Fake"}, {"workshop_state", "subscribed_missing"}}}}});
    QVERIFY(!m_model->data(idx, RemoteCatalogModel::SubscriptionPendingRole).toBool());
    QVERIFY(m_model->data(idx, RemoteCatalogModel::SubscribedRole).toBool());

    // An error clears pending immediately and surfaces the message.
    m_local->replaceFromCatalog(QJsonObject{{"items", QJsonArray{}}});
    fail = true;
    m_model->subscribe("501");
    QTRY_VERIFY(!m_model->data(idx, RemoteCatalogModel::SubscriptionPendingRole).toBool());
    QCOMPARE(m_model->notice(), QStringLiteral("Steam is limiting requests. Try again in a minute."));
  }

  void acceptedButNeverInstalledTimesOut() {
    m_daemon.handler = [](const QString &method, const QJsonObject &) {
      if (method == "workshop.query")
        return page({"601"}, false);
      return Fake::okAnswer({{"accepted", true}});
    };
    m_model->setPendingTimeout(100);
    m_model->refresh();
    QTRY_COMPARE(m_model->state(), RemoteCatalogModel::Ready);
    m_model->subscribe("601");
    QTRY_VERIFY(!m_model->data(m_model->index(0), RemoteCatalogModel::SubscriptionPendingRole).toBool());
    QVERIFY(m_model->notice().contains("Steam client"));
  }

  // W5: the Subscribed view's details pane unsubscribes items that are not
  // Browse rows; pendingIds carries the state until the local catalog drops
  // the item, and a failure clears it with a notice.
  void unsubscribeWithoutBrowseRowTracksPendingIds() {
    bool fail = false;
    m_daemon.handler = [&fail](const QString &, const QJsonObject &) {
      return fail ? Fake::errorAnswer("signed_out") : Fake::okAnswer({{"accepted", true}});
    };
    const QJsonObject installed{{"items", QJsonArray{QJsonObject{
        {"workshop_id", "801"}, {"title", "Fake"}, {"workshop_state", "subscribed_installed"}}}}};
    m_local->replaceFromCatalog(installed);
    QSignalSpy pendingSpy(m_model.get(), &RemoteCatalogModel::pendingIdsChanged);
    QCOMPARE(m_model->rowCount(), 0);

    m_model->unsubscribe("801");
    QCOMPARE(m_model->pendingIds(), QStringList{"801"});
    QCOMPARE(pendingSpy.count(), 1);
    QTRY_COMPARE(m_daemon.count("workshop.unsubscribe"), 1);
    QCOMPARE(m_daemon.paramsLog.last().value("id").toString(), QStringLiteral("801"));
    QTest::qWait(50);
    // Steam accepted, but the item is still on disk: still pending, and a
    // second click is ignored rather than sent twice.
    QCOMPARE(m_model->pendingIds(), QStringList{"801"});
    m_model->unsubscribe("801");
    QTest::qWait(50);
    QCOMPARE(m_daemon.count("workshop.unsubscribe"), 1);

    // Steam removed it: the next catalog clears the pending state quietly.
    m_local->replaceFromCatalog(QJsonObject{{"items", QJsonArray{}}});
    QVERIFY(m_model->pendingIds().isEmpty());
    QCOMPARE(pendingSpy.count(), 2);
    QVERIFY(m_model->notice().isEmpty());

    // Failure: cleared immediately, with the reason as the notice.
    m_local->replaceFromCatalog(installed);
    fail = true;
    m_model->unsubscribe("801");
    QTRY_VERIFY(m_model->pendingIds().isEmpty());
    QCOMPARE(m_model->notice(), QStringLiteral("You are signed out of Steam. Sign in to continue."));
    // Review fixes C2: the notice names its item, so another item's
    // details pane does not show it.
    QCOMPARE(m_model->noticeId(), QStringLiteral("801"));

    // Invalid ids never reach the daemon or the pending list.
    m_model->unsubscribe("0");
    m_model->unsubscribe("abc");
    QVERIFY(m_model->pendingIds().isEmpty());
  }

  void detailsEmitsDescription() {
    m_daemon.handler = [](const QString &, const QJsonObject &) {
      QJsonObject d = item("701");
      d.insert("description", "Long fake description");
      return Fake::okAnswer(d);
    };
    QSignalSpy spy(m_model.get(), &RemoteCatalogModel::detailsReady);
    m_model->details("701");
    QTRY_COMPARE(spy.count(), 1);
    QCOMPARE(spy.first().at(1).toString(), QStringLiteral("Long fake description"));
  }
};

QTEST_GUILESS_MAIN(RemoteCatalogModelTest)
#include "remotecatalogmodeltest.moc"
