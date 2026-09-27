// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

#include <KScreenDpms/Dpms>

#include <QObject>
#include <QSet>
#include <QString>

class QScreen;

// Tracks which screens the compositor has powered down and renders that
// view as the {"outputs":[...],"asleep":[...]} report the relay forwards.
// A screen whose power state was never announced counts as awake, so a
// helper started while the displays sleep errs toward rendering.
class DisplayPowerWatcher final : public QObject {
    Q_OBJECT

public:
    explicit DisplayPowerWatcher(QObject *parent = nullptr);

    bool supported() const { return m_dpms.isSupported(); }
    QString report() const;

Q_SIGNALS:
    void reportChanged(const QString &json);

private:
    void onModeChanged(KScreen::Dpms::Mode mode, QScreen *screen);
    void publish();

    KScreen::Dpms m_dpms;
    QSet<QScreen *> m_asleep;
    QString m_lastReport;
};
