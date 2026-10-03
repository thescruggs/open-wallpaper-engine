// SPDX-License-Identifier: GPL-3.0-or-later
#include "../src/qrcodeitem.h"

#include <QtTest>

class QrCodeItemTest final : public QObject {
  Q_OBJECT
private slots:
  void shortTextIsVersionOne() {
    const auto matrix = QrCodeItem::encode(QStringLiteral("HELLO"));
    QCOMPARE(matrix.size, 21);  // QR version 1
    QCOMPARE(matrix.modules.size(), 21 * 21);
    // The top-left finder pattern: dark corner, light ring inside it.
    QVERIFY(matrix.dark(0, 0));
    QVERIFY(matrix.dark(6, 0));
    QVERIFY(!matrix.dark(1, 1));
    QVERIFY(matrix.dark(2, 2));
    int dark = 0;
    for (char c : matrix.modules)
      dark += c != 0;
    QVERIFY(dark > 50 && dark < 21 * 21 - 50);
  }
  void longerTextGrows() {
    const auto big = QrCodeItem::encode(QStringLiteral("https://s.team/q/1/0000000000000000000"));
    QVERIFY(big.size > 21);
    QCOMPARE((big.size - 17) % 4, 0);
  }
  void emptyTextIsEmpty() {
    QVERIFY(QrCodeItem::encode(QString()).isEmpty());
  }
};

QTEST_GUILESS_MAIN(QrCodeItemTest)
#include "qrcodeitemtest.moc"
