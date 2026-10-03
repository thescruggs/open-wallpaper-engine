// SPDX-License-Identifier: GPL-3.0-or-later
#include "workshopclient.h"

#include <QDesktopServices>
#include <QJsonDocument>
#include <QLocalSocket>
#include <QRegularExpression>
#include <QUrl>
#include <memory>

namespace {
constexpr qsizetype MaxResponseBytes = 6 * 1024 * 1024;
constexpr qsizetype MaxDetailChars = 200;
constexpr int BusyRetryMilliseconds = 600;
constexpr int kAuthBusyRetries = 8;

// One in-flight request. Shared between the socket signal lambdas and the
// deadline timer; `done` makes completion idempotent.
struct Exchange {
  QLocalSocket *socket = nullptr;
  QByteArray buffer;
  bool done = false;
};
}

WorkshopClient::WorkshopClient(QString socketPath, QObject *parent)
    : QObject(parent), m_socketPath(std::move(socketPath)) {
  m_pollTimer.setSingleShot(true);
  connect(&m_pollTimer, &QTimer::timeout, this, &WorkshopClient::pollNow);
}

bool WorkshopClient::openItem(const QString &workshopId) {
  static const QRegularExpression idPattern(QStringLiteral("^[0-9]{1,20}$"));
  if (!idPattern.match(workshopId).hasMatch() || workshopId.toULongLong() == 0) {
    setError(tr("The Workshop ID is invalid."));
    return false;
  }
  const QUrl url(QStringLiteral("steam://url/CommunityFilePage/%1").arg(workshopId));
  if (!QDesktopServices::openUrl(url)) {
    setError(tr("Steam could not be opened for Workshop item %1.").arg(workshopId));
    return false;
  }
  setError({});
  return true;
}

QString WorkshopClient::describeError(const QString &code, const QString &detail) {
  if (code == QLatin1String("offline"))
    return tr("Steam is unreachable. Check your network connection.");
  if (code == QLatin1String("rate_limited"))
    return tr("Steam is limiting requests. Try again in a minute.");
  if (code == QLatin1String("signed_out"))
    return tr("You are signed out of Steam. Sign in to continue.");
  if (code == QLatin1String("worker_unavailable"))
    return tr("The Workshop component is not installed, so Steam browsing is unavailable. "
              "You can still open items in the Steam client.");
  if (code == QLatin1String("timeout"))
    return tr("Steam did not answer in time. Try again.");
  if (code == QLatin1String("connect_failed"))
    return tr("Could not reach the wallpaper service.");
  if (code == QLatin1String("busy"))
    return tr("Another Workshop request is still running. Try again in a moment.");
  QString text = detail.simplified();
  if (text.size() > MaxDetailChars)
    text = text.left(MaxDetailChars) + QStringLiteral("…");
  if (text.isEmpty())
    return tr("The Workshop request failed (%1).").arg(code.left(40));
  return tr("The Workshop request failed: %1").arg(text);
}

void WorkshopClient::call(const QString &method, const QJsonObject &params, QObject *context,
                          Callback callback, int busyRetries) {
  sendRequest(method, params, context, std::move(callback), busyRetries);
}

void WorkshopClient::sendRequest(const QString &method, const QJsonObject &params,
                                 QObject *context, Callback callback, int busyRetriesLeft) {
  auto exchange = std::make_shared<Exchange>();
  exchange->socket = new QLocalSocket(this);
  QPointer<QObject> guard(context);
  const bool hasContext = context != nullptr;
  QPointer<WorkshopClient> self(this);

  auto finish = [exchange, guard, hasContext, callback, self, method, params,
                 busyRetriesLeft](Reply reply) {
    if (exchange->done)
      return;
    exchange->done = true;
    exchange->socket->abort();
    exchange->socket->deleteLater();
    if (!self)
      return;
    if (hasContext && guard.isNull())
      return;
    if (!reply.ok && reply.error == QLatin1String("busy") && busyRetriesLeft > 0) {
      QTimer::singleShot(BusyRetryMilliseconds, self.data(),
                         [self, method, params, guard, hasContext, callback, busyRetriesLeft] {
        if (!self || (hasContext && guard.isNull()))
          return;
        self->sendRequest(method, params, guard.data(), callback, busyRetriesLeft - 1);
      });
      return;
    }
    if (!reply.ok)
      self->applyAccountError(reply.error);
    if (callback)
      callback(reply);
  };

  auto *socket = exchange->socket;
  const quint64 serial = ++m_requestSerial;
  connect(socket, &QLocalSocket::connected, this, [socket, method, params, serial] {
    const QJsonObject request{{QStringLiteral("version"), 1},
                              {QStringLiteral("id"), static_cast<qint64>(serial)},
                              {QStringLiteral("method"), method},
                              {QStringLiteral("params"), params}};
    socket->write(QJsonDocument(request).toJson(QJsonDocument::Compact) + '\n');
  });
  connect(socket, &QLocalSocket::errorOccurred, this,
          [finish, exchange](QLocalSocket::LocalSocketError) {
    if (exchange->done)
      return;
    Reply reply;
    reply.error = QStringLiteral("connect_failed");
    finish(reply);
  });
  connect(socket, &QLocalSocket::readyRead, this, [finish, exchange, socket] {
    exchange->buffer += socket->readAll();
    Reply reply;
    if (exchange->buffer.size() > MaxResponseBytes) {
      reply.error = QStringLiteral("bad_response");
      finish(reply);
      return;
    }
    const auto newline = exchange->buffer.indexOf('\n');
    if (newline < 0)
      return;
    QJsonParseError parseError;
    const auto document = QJsonDocument::fromJson(exchange->buffer.left(newline), &parseError);
    if (parseError.error != QJsonParseError::NoError || !document.isObject()) {
      reply.error = QStringLiteral("bad_response");
      finish(reply);
      return;
    }
    const auto response = document.object();
    const auto result = response.value(QStringLiteral("result")).toObject();
    reply.ok = response.value(QStringLiteral("ok")).toBool(
        !response.contains(QStringLiteral("error")) && !result.contains(QStringLiteral("error")));
    if (reply.ok) {
      reply.result = result;
    } else {
      // The error object sits in `result` (like every other daemon RPC) or,
      // per the contract's wording, at the top level; accept both.
      const auto &source = result.contains(QStringLiteral("error")) ? result : response;
      reply.error = source.value(QStringLiteral("error")).toString(QStringLiteral("failed"));
      reply.detail = source.value(QStringLiteral("detail")).toString();
    }
    finish(reply);
  });
  QTimer::singleShot(m_deadlineMilliseconds, socket, [finish, exchange] {
    if (exchange->done)
      return;
    Reply reply;
    reply.error = QStringLiteral("timeout");
    finish(reply);
  });
  socket->connectToServer(m_socketPath, QIODevice::ReadWrite);
}

void WorkshopClient::applyAccountError(const QString &code) {
  if (code == QLatin1String("signed_out")) {
    m_signInPending = false;
    m_pollTimer.stop();
    ++m_signInGeneration;
    setAccountName({});
    setChallengeUrl({});
    setAuthError({});
    setAuthState(SignedOut);
  } else if (code == QLatin1String("worker_unavailable")) {
    m_signInPending = false;
    m_pollTimer.stop();
    ++m_signInGeneration;
    setChallengeUrl({});
    setAuthError(describeError(code, {}));
    setAuthState(WorkerUnavailable);
  }
}

void WorkshopClient::refreshAuthStatus() {
  if (m_signInPending)
    return;
  call(QStringLiteral("workshop.auth.status"), {}, this, [this](const Reply &reply) {
    if (m_signInPending)
      return;
    if (!reply.ok) {
      if (reply.error != QLatin1String("signed_out") &&
          reply.error != QLatin1String("worker_unavailable")) {
        setAuthError(describeError(reply.error, reply.detail));
        setAuthState(Failed);
      }
      return;
    }
    if (!reply.result.value(QStringLiteral("worker_available")).toBool(true)) {
      setAuthError(describeError(QStringLiteral("worker_unavailable"), {}));
      setAuthState(WorkerUnavailable);
    } else if (reply.result.value(QStringLiteral("signed_in")).toBool()) {
      setAccountName(reply.result.value(QStringLiteral("account_name")).toString().left(128));
      setAuthError({});
      setAuthState(SignedIn);
    } else {
      setAccountName({});
      setAuthError({});
      setAuthState(SignedOut);
    }
  }, kAuthBusyRetries);
}

// A sign-in can complete in the worker while this client stopped listening
// (cancel raced an approving poll, or the daemon timed the worker out after
// the credential was stored). Ask once and adopt it; leave the state alone
// otherwise.
void WorkshopClient::adoptExistingSignIn() {
  call(QStringLiteral("workshop.auth.status"), {}, this, [this](const Reply &reply) {
    if (m_signInPending || !reply.ok ||
        !reply.result.value(QStringLiteral("signed_in")).toBool())
      return;
    setAccountName(reply.result.value(QStringLiteral("account_name")).toString().left(128));
    setChallengeUrl({});
    setAuthError({});
    setAuthState(SignedIn);
  }, kAuthBusyRetries);
}

void WorkshopClient::beginSignIn() {
  m_restarts = 0;
  startBegin(true);
}

void WorkshopClient::startBegin(bool userInitiated) {
  Q_UNUSED(userInitiated)
  m_pollTimer.stop();
  m_signInPending = true;
  m_pollInFlight = false;
  const quint64 generation = ++m_signInGeneration;
  setAuthError({});
  setChallengeUrl({});
  setAuthState(Starting);
  call(QStringLiteral("workshop.auth.begin"), {}, this, [this, generation](const Reply &reply) {
    if (generation != m_signInGeneration)
      return;
    if (!reply.ok) {
      if (reply.error == QLatin1String("signed_out") ||
          reply.error == QLatin1String("worker_unavailable"))
        return;  // applyAccountError already set the state
      failSignIn(describeError(reply.error, reply.detail));
      return;
    }
    const QString url = reply.result.value(QStringLiteral("challenge_url")).toString();
    if (url.isEmpty()) {
      failSignIn(tr("Steam did not return a sign-in code. Try again."));
      return;
    }
    m_pollIntervalMs = qBound(m_pollMinimumMs,
                              reply.result.value(QStringLiteral("poll_interval_ms")).toInt(2000),
                              m_pollMaximumMs);
    setChallengeUrl(url);
    setAuthState(AwaitingScan);
    schedulePoll();
  }, kAuthBusyRetries);
}

void WorkshopClient::schedulePoll() {
  if (m_signInPending)
    m_pollTimer.start(m_pollIntervalMs);
}

void WorkshopClient::pollNow() {
  if (!m_signInPending || m_pollInFlight)
    return;
  m_pollInFlight = true;
  const quint64 generation = m_signInGeneration;
  call(QStringLiteral("workshop.auth.poll"), {}, this,
       [this, generation](const Reply &reply) { handlePoll(reply, generation); });
}

void WorkshopClient::handlePoll(const Reply &reply, quint64 generation) {
  if (generation != m_signInGeneration)
    return;
  m_pollInFlight = false;
  if (!reply.ok) {
    if (reply.error == QLatin1String("busy")) {
      schedulePoll();  // not an error: ask again next tick
      return;
    }
    if (reply.error == QLatin1String("signed_out") ||
        reply.error == QLatin1String("worker_unavailable"))
      return;
    failSignIn(describeError(reply.error, reply.detail));
    adoptExistingSignIn();
    return;
  }
  const QString state = reply.result.value(QStringLiteral("state")).toString();
  const QString url = reply.result.value(QStringLiteral("challenge_url")).toString();
  if (state == QLatin1String("signed_in")) {
    m_signInPending = false;
    m_pollTimer.stop();
    setChallengeUrl({});
    setAccountName(reply.result.value(QStringLiteral("account_name")).toString().left(128));
    setAuthError({});
    setAuthState(SignedIn);
  } else if (state == QLatin1String("expired")) {
    if (m_restarts < MaxSignInRestarts) {
      ++m_restarts;
      startBegin(false);
    } else {
      failSignIn(tr("The sign-in code expired several times. Try again."));
    }
  } else if (state == QLatin1String("awaiting_confirmation")) {
    setAuthState(AwaitingConfirmation);
    schedulePoll();
  } else if (state == QLatin1String("challenge_changed")) {
    if (!url.isEmpty())
      setChallengeUrl(url);
    setAuthState(AwaitingScan);
    schedulePoll();
  } else {  // awaiting_scan and anything unknown
    if (!url.isEmpty() && m_authState == AwaitingScan)
      setChallengeUrl(url);
    schedulePoll();
  }
}

void WorkshopClient::failSignIn(const QString &message) {
  m_signInPending = false;
  m_pollInFlight = false;
  m_pollTimer.stop();
  setChallengeUrl({});
  setAuthError(message);
  setAuthState(Failed);
}

void WorkshopClient::cancelSignIn() {
  const bool wasPending = m_signInPending;
  m_signInPending = false;
  m_pollInFlight = false;
  m_pollTimer.stop();
  ++m_signInGeneration;
  if (!wasPending)
    return;
  setChallengeUrl({});
  setAuthError({});
  setAuthState(SignedOut);
  call(QStringLiteral("workshop.auth.cancel"), {}, this,
       [this](const Reply &) { adoptExistingSignIn(); });
}

void WorkshopClient::signOut() {
  call(QStringLiteral("workshop.auth.sign_out"), {}, this, [this](const Reply &reply) {
    if (!reply.ok) {
      if (reply.error != QLatin1String("signed_out") &&
          reply.error != QLatin1String("worker_unavailable")) {
        setAuthError(describeError(reply.error, reply.detail));
        setAuthState(Failed);
      }
      return;
    }
    setAccountName({});
    setChallengeUrl({});
    setAuthError({});
    setAuthState(SignedOut);
  }, kAuthBusyRetries);
}

void WorkshopClient::setError(const QString &message) {
  if (m_errorMessage == message)
    return;
  m_errorMessage = message;
  emit errorMessageChanged();
}

void WorkshopClient::setAuthState(AuthState state) {
  if (m_authState == state)
    return;
  m_authState = state;
  emit authStateChanged();
}

void WorkshopClient::setAccountName(const QString &name) {
  if (m_accountName == name)
    return;
  m_accountName = name;
  emit accountNameChanged();
}

void WorkshopClient::setChallengeUrl(const QString &url) {
  if (m_challengeUrl == url)
    return;
  m_challengeUrl = url;
  emit challengeUrlChanged();
}

void WorkshopClient::setAuthError(const QString &message) {
  if (m_authError == message)
    return;
  m_authError = message;
  emit authErrorChanged();
}
