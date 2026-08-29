// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

#include <QObject>
#include <QString>

// Per-renderer-kind apply defaults (scene/video/web: frame rate limit and
// scaling mode), stored in QSettings. They preselect the Apply controls in
// the details pane; each apply still persists its own values with the
// output's assignment, which playlists reuse on later switches.
class RendererDefaults final : public QObject {
    Q_OBJECT
public:
    explicit RendererDefaults(QObject *parent = nullptr);
    Q_INVOKABLE int fps(const QString &kind) const;
    Q_INVOKABLE QString scaling(const QString &kind) const;
    Q_INVOKABLE void setFps(const QString &kind, int value);
    Q_INVOKABLE void setScaling(const QString &kind, const QString &value);

signals:
    void changed();
};
