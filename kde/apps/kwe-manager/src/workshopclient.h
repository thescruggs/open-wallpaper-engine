// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

#include <QJsonObject>
#include <QObject>
#include <QPointer>
#include <QQmlEngine>
#include <QString>
#include <QTimer>
#include <functional>

// Steam Workshop access for the manager.
//
// Two jobs:
//  * openItem(): the steam:// deep link, which is also the signed-out
//    fallback of every Workshop action.
//  * the daemon-backed workshop.* RPC client (docs/WORKSHOP_PROTOCOL_V1.md
//    section 2) and the QR sign-in state machine on top of it. The manager
//    never sees a token: sign-in is driven entirely by challenge URLs and
//    poll states the daemon relays.
//
// Transport mirrors SettingsClient: one request per connection, newline
// delimited JSON. Unlike SettingsClient every call owns its socket, because
// workshop.* answers can take up to 45 s and must not block each other; the
// daemon serializes per family and answers "busy" when it has to.
class WorkshopClient : public QObject {
  Q_OBJECT
  QML_ELEMENT
  QML_UNCREATABLE("WorkshopClient is created by the manager")
  Q_PROPERTY(QString errorMessage READ errorMessage NOTIFY errorMessageChanged)
  Q_PROPERTY(AuthState authState READ authState NOTIFY authStateChanged)
  Q_PROPERTY(QString accountName READ accountName NOTIFY accountNameChanged)
  Q_PROPERTY(QString challengeUrl READ challengeUrl NOTIFY challengeUrlChanged)
  Q_PROPERTY(QString authError READ authError NOTIFY authErrorChanged)

public:
  enum AuthState {
    Unknown,
    WorkerUnavailable,
    SignedOut,
    Starting,
    AwaitingScan,
    AwaitingConfirmation,
    SignedIn,
    Failed
  };
  Q_ENUM(AuthState)

  struct Reply {
    bool ok = false;
    QString error;   // daemon error code, or "timeout" / "connect_failed" / "bad_response"
    QString detail;  // daemon detail text (untrusted)
    QJsonObject result;
  };
  using Callback = std::function<void(const Reply &)>;

  static constexpr int RequestDeadlineMilliseconds = 60000;
  static constexpr int MaxSignInRestarts = 3;

  explicit WorkshopClient(QString socketPath = {}, QObject *parent = nullptr);

  QString errorMessage() const { return m_errorMessage; }
  AuthState authState() const { return m_authState; }
  QString accountName() const { return m_accountName; }
  QString challengeUrl() const { return m_challengeUrl; }
  QString authError() const { return m_authError; }

  Q_INVOKABLE bool openItem(const QString &workshopId);

  Q_INVOKABLE void refreshAuthStatus();
  Q_INVOKABLE void beginSignIn();
  Q_INVOKABLE void cancelSignIn();
  Q_INVOKABLE void signOut();

  // Generic workshop.* call; `callback` runs on the GUI thread unless
  // `context` was destroyed meanwhile. Errors that change the account state
  // (signed_out, worker_unavailable) are applied here so every caller keeps
  // authState truthful. A "busy" answer is retried up to `busyRetries`
  // times, 600 ms apart.
  void call(const QString &method, const QJsonObject &params, QObject *context,
            Callback callback, int busyRetries = 0);

  // Plain-text, length-capped message for a daemon error code.
  static QString describeError(const QString &code, const QString &detail);

  // Test hooks.
  void setRequestDeadline(int milliseconds) { m_deadlineMilliseconds = milliseconds; }
  void setPollIntervalBounds(int minimumMs, int maximumMs) {
    m_pollMinimumMs = minimumMs;
    m_pollMaximumMs = maximumMs;
  }

signals:
  void errorMessageChanged();
  void authStateChanged();
  void accountNameChanged();
  void challengeUrlChanged();
  void authErrorChanged();

private:
  void setError(const QString &message);
  void setAuthState(AuthState state);
  void setAccountName(const QString &name);
  void setChallengeUrl(const QString &url);
  void setAuthError(const QString &message);
  void startBegin(bool userInitiated);
  void schedulePoll();
  void pollNow();
  void handlePoll(const Reply &reply, quint64 generation);
  void failSignIn(const QString &message);
  void adoptExistingSignIn();
  void applyAccountError(const QString &code);
  void sendRequest(const QString &method, const QJsonObject &params, QObject *context,
                   Callback callback, int busyRetriesLeft);

  QString m_socketPath;
  QString m_errorMessage;
  AuthState m_authState = Unknown;
  QString m_accountName;
  QString m_challengeUrl;
  QString m_authError;
  int m_deadlineMilliseconds = RequestDeadlineMilliseconds;
  int m_pollMinimumMs = 1000;
  int m_pollMaximumMs = 10000;
  int m_pollIntervalMs = 2000;
  int m_restarts = 0;
  bool m_signInPending = false;
  bool m_pollInFlight = false;
  quint64 m_signInGeneration = 0;
  quint64 m_requestSerial = 0;
  QTimer m_pollTimer;
};
