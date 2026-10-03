// SPDX-License-Identifier: GPL-3.0-or-later
#include "qrcodeitem.h"

#include <QPainter>
#include <qrencode.h>

QrCodeItem::Matrix QrCodeItem::encode(const QString &text) {
  Matrix matrix;
  if (text.isEmpty())
    return matrix;
  const QByteArray utf8 = text.toUtf8();
  QRcode *code = QRcode_encodeString8bit(utf8.constData(), 0, QR_ECLEVEL_M);
  if (!code)
    return matrix;
  matrix.size = code->width;
  matrix.modules.resize(code->width * code->width);
  for (int i = 0; i < code->width * code->width; ++i)
    matrix.modules[i] = (code->data[i] & 1) ? 1 : 0;
  QRcode_free(code);
  return matrix;
}

QrCodeItem::QrCodeItem(QQuickItem *parent) : QQuickPaintedItem(parent) {
  setAntialiasing(false);
}

void QrCodeItem::setText(const QString &text) {
  if (m_text == text)
    return;
  m_text = text;
  m_matrix = encode(text);
  emit textChanged();
  update();
}

void QrCodeItem::paint(QPainter *painter) {
  if (m_matrix.isEmpty())
    return;
  const int modules = m_matrix.size + 2 * QuietZoneModules;
  const int side = static_cast<int>(qMin(width(), height()));
  const int module = qMax(1, side / modules);
  const int total = module * modules;
  const int left = (static_cast<int>(width()) - total) / 2;
  const int top = (static_cast<int>(height()) - total) / 2;
  painter->setRenderHint(QPainter::Antialiasing, false);
  painter->setRenderHint(QPainter::SmoothPixmapTransform, false);
  painter->fillRect(left, top, total, total, Qt::white);
  for (int y = 0; y < m_matrix.size; ++y)
    for (int x = 0; x < m_matrix.size; ++x)
      if (m_matrix.dark(x, y))
        painter->fillRect(left + (x + QuietZoneModules) * module,
                          top + (y + QuietZoneModules) * module, module, module, Qt::black);
}
