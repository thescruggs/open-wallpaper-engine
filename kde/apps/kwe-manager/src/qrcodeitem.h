// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

#include <QByteArray>
#include <QQmlEngine>
#include <QQuickPaintedItem>
#include <QString>

// Paints a QR code for `text`. Modules are always black on a white quiet
// zone, whatever the theme: scanners need that contrast. Each module is an
// integer number of device-independent pixels so edges stay crisp.
class QrCodeItem : public QQuickPaintedItem {
  Q_OBJECT
  QML_ELEMENT
  Q_PROPERTY(QString text READ text WRITE setText NOTIFY textChanged)

public:
  // Module matrix of a QR symbol: size x size, row-major, 1 = dark.
  struct Matrix {
    int size = 0;
    QByteArray modules;
    bool isEmpty() const { return size == 0; }
    bool dark(int x, int y) const { return modules.at(y * size + x) != 0; }
  };
  static constexpr int QuietZoneModules = 4;
  // Empty matrix for empty text or when the text cannot be encoded.
  static Matrix encode(const QString &text);

  explicit QrCodeItem(QQuickItem *parent = nullptr);
  QString text() const { return m_text; }
  void setText(const QString &text);
  void paint(QPainter *painter) override;

signals:
  void textChanged();

private:
  QString m_text;
  Matrix m_matrix;
};
