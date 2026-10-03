// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

#include <QJsonDocument>
#include <QJsonObject>
#include <QLocalServer>
#include <QLocalSocket>
#include <QPointer>
#include <functional>

// Daemon stand-in for the workshop.* wire protocol. The handler decides each
// answer; a held request stays open until release() so tests can reorder
// answers.
class FakeWorkshopDaemon final : public QObject {
  Q_OBJECT
public:
  struct Answer {
    bool hold = false;
    bool ok = true;
    QJsonObject result;
  };
  struct Held {
    QPointer<QLocalSocket> socket;
    QJsonObject request;
  };
  static Answer okAnswer(const QJsonObject &result = {}) { return {false, true, result}; }
  static Answer errorAnswer(const QString &code, const QString &detail = {}) {
    return {false, false, QJsonObject{{QStringLiteral("error"), code}, {QStringLiteral("detail"), detail}}};
  }
  static Answer holdAnswer() { return {true, true, {}}; }

  explicit FakeWorkshopDaemon(QObject *parent = nullptr) : QObject(parent) {
    connect(&m_server, &QLocalServer::newConnection, this, &FakeWorkshopDaemon::onConnection);
  }
  bool listen(const QString &path) { return m_server.listen(path); }

  std::function<Answer(const QString &method, const QJsonObject &params)> handler;
  QStringList methods;                 // every request, in arrival order
  QList<QJsonObject> paramsLog;        // params of each request
  QList<Held> held;

  int count(const QString &method) const { return methods.count(method); }

  void release(int index, const Answer &answer) {
    const Held h = held.at(index);
    if (h.socket)
      reply(h.socket, h.request, answer);
  }

private:
  QLocalServer m_server;

  static void reply(QLocalSocket *socket, const QJsonObject &request, const Answer &answer) {
    socket->write(QJsonDocument(QJsonObject{{QStringLiteral("version"), 1},
                                            {QStringLiteral("id"), request.value(QStringLiteral("id"))},
                                            {QStringLiteral("ok"), answer.ok},
                                            {QStringLiteral("result"), answer.result}})
                      .toJson(QJsonDocument::Compact) + '\n');
    socket->flush();
  }

  void onConnection() {
    auto *socket = m_server.nextPendingConnection();
    connect(socket, &QLocalSocket::readyRead, this, [this, socket] {
      const auto newline = socket->peek(64 * 1024).indexOf('\n');
      if (newline < 0)
        return;
      const auto request = QJsonDocument::fromJson(socket->read(newline + 1)).object();
      const QString method = request.value(QStringLiteral("method")).toString();
      const QJsonObject params = request.value(QStringLiteral("params")).toObject();
      methods.push_back(method);
      paramsLog.push_back(params);
      const Answer answer = handler ? handler(method, params) : errorAnswer(QStringLiteral("internal"));
      if (answer.hold) {
        held.push_back({socket, request});
        return;
      }
      reply(socket, request, answer);
    });
  }
};
