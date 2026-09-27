// SPDX-License-Identifier: GPL-3.0-or-later
#include "displaypowerwatcher.h"

#include <QGuiApplication>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QScreen>

#include <cstdio>

DisplayPowerWatcher::DisplayPowerWatcher(QObject *parent) : QObject(parent) {
    connect(&m_dpms, &KScreen::Dpms::modeChanged, this, &DisplayPowerWatcher::onModeChanged);
    connect(&m_dpms, &KScreen::Dpms::supportedChanged, this, [this](bool supported) {
        fprintf(stderr, "event=display_power.supported value=%d\n", int(supported));
        if (!supported)
            m_asleep.clear();
        publish();
    });
    connect(qGuiApp, &QGuiApplication::screenAdded, this, [this](QScreen *) { publish(); });
    connect(qGuiApp, &QGuiApplication::screenRemoved, this, [this](QScreen *screen) {
        m_asleep.remove(screen);
        publish();
    });
}

void DisplayPowerWatcher::onModeChanged(KScreen::Dpms::Mode mode, QScreen *screen) {
    if (!screen)
        return;
    // Standby, suspend and off all mean "nothing is being shown".
    if (mode == KScreen::Dpms::On)
        m_asleep.remove(screen);
    else
        m_asleep.insert(screen);
    publish();
}

QString DisplayPowerWatcher::report() const {
    QStringList outputs;
    QStringList asleep;
    const auto screens = QGuiApplication::screens();
    for (const auto *screen : screens) {
        // The placeholder Qt keeps while no output exists has no name.
        const auto name = screen->name();
        if (name.isEmpty() || outputs.contains(name))
            continue;
        outputs.push_back(name);
        if (m_asleep.contains(const_cast<QScreen *>(screen)))
            asleep.push_back(name);
    }
    outputs.sort();
    asleep.sort();
    return QString::fromUtf8(
        QJsonDocument(QJsonObject{{QStringLiteral("outputs"), QJsonArray::fromStringList(outputs)},
                                  {QStringLiteral("asleep"), QJsonArray::fromStringList(asleep)}})
            .toJson(QJsonDocument::Compact));
}

void DisplayPowerWatcher::publish() {
    const auto json = report();
    if (json == m_lastReport)
        return;
    m_lastReport = json;
    emit reportChanged(json);
}
