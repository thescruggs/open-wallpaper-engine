// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

#include <QObject>
#include <QString>

class SettingsClient;

// Per-renderer-kind apply defaults (scene/video/web: frame rate limit and
// scaling mode) as the QML side sees them. The values live in the daemon
// (settings.renderer_defaults, via SettingsClient): every apply lane and
// every playlist advance honours them, and the details pane's Apply controls
// preselect from them. The first time the daemon record is available, values
// a previous release stored in this user's QSettings are migrated into it
// (only while the daemon still holds the built-ins) and then removed.
class RendererDefaults final : public QObject {
    Q_OBJECT
    // Bumps on every change so QML can re-read fps()/scaling() for a kind.
    Q_PROPERTY(int revision READ revision NOTIFY changed)
public:
    explicit RendererDefaults(SettingsClient *client, QObject *parent = nullptr);
    int revision() const { return m_revision; }
    Q_INVOKABLE int fps(const QString &kind) const;
    Q_INVOKABLE QString scaling(const QString &kind) const;
    Q_INVOKABLE void setFps(const QString &kind, int value);
    Q_INVOKABLE void setScaling(const QString &kind, const QString &value);

signals:
    void changed();

private:
    void migrateLegacySettings();

    SettingsClient *m_client;
    int m_revision = 0;
    bool m_migrated = false;
};
