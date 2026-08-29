// SPDX-License-Identifier: GPL-3.0-or-later
#include "../src/rendererdefaults.h"

#include <QCoreApplication>
#include <QSettings>
#include <QTemporaryDir>
#include <QtTest>

class RendererDefaultsTest final : public QObject {
    Q_OBJECT

private slots:
    void initTestCase() {
        QVERIFY(m_settingsRoot.isValid());
        QCoreApplication::setOrganizationName(QStringLiteral("KDEWallpaperEngineTests"));
        QCoreApplication::setApplicationName(QStringLiteral("RendererDefaults"));
        QSettings::setDefaultFormat(QSettings::IniFormat);
        QSettings::setPath(QSettings::IniFormat, QSettings::UserScope, m_settingsRoot.path());
    }

    void init() {
        QSettings settings;
        settings.clear();
        settings.sync();
    }

    void storesAndRecallsPerKind() {
        RendererDefaults defaults;
        QSignalSpy changedSpy(&defaults, &RendererDefaults::changed);
        QCOMPARE(defaults.fps(QStringLiteral("scene")), 30);
        QCOMPARE(defaults.scaling(QStringLiteral("scene")), QStringLiteral("aspect"));

        defaults.setFps(QStringLiteral("scene"), 60);
        defaults.setScaling(QStringLiteral("video"), QStringLiteral("fill"));
        QCOMPARE(changedSpy.count(), 2);
        QCOMPARE(defaults.fps(QStringLiteral("scene")), 60);
        QCOMPARE(defaults.scaling(QStringLiteral("video")), QStringLiteral("fill"));
        // Other kinds keep their own defaults.
        QCOMPARE(defaults.fps(QStringLiteral("video")), 30);
        QCOMPARE(defaults.scaling(QStringLiteral("scene")), QStringLiteral("aspect"));

        // Values survive a fresh instance (QSettings-backed).
        RendererDefaults reloaded;
        QCOMPARE(reloaded.fps(QStringLiteral("scene")), 60);
        QCOMPARE(reloaded.scaling(QStringLiteral("video")), QStringLiteral("fill"));
    }

    void rejectsInvalidInputAndValues() {
        RendererDefaults defaults;
        QSignalSpy changedSpy(&defaults, &RendererDefaults::changed);
        defaults.setFps(QStringLiteral("scene"), 0);
        defaults.setFps(QStringLiteral("scene"), 241);
        defaults.setFps(QStringLiteral("toaster"), 30);
        defaults.setScaling(QStringLiteral("scene"), QStringLiteral("sideways"));
        defaults.setScaling(QStringLiteral("toaster"), QStringLiteral("fill"));
        QCOMPARE(changedSpy.count(), 0);
        QCOMPARE(defaults.fps(QStringLiteral("scene")), 30);
        QCOMPARE(defaults.scaling(QStringLiteral("scene")), QStringLiteral("aspect"));
        QCOMPARE(defaults.fps(QStringLiteral("toaster")), 30);

        // A hand-edited settings file with out-of-bounds values falls back.
        QSettings settings;
        settings.setValue(QStringLiteral("renderer-defaults/web/fps"), 100000);
        settings.setValue(QStringLiteral("renderer-defaults/web/scaling"), QStringLiteral("huge"));
        settings.sync();
        QCOMPARE(defaults.fps(QStringLiteral("web")), 30);
        QCOMPARE(defaults.scaling(QStringLiteral("web")), QStringLiteral("aspect"));
    }

private:
    QTemporaryDir m_settingsRoot;
};

QTEST_GUILESS_MAIN(RendererDefaultsTest)
#include "rendererdefaultstest.moc"
