// SPDX-License-Identifier: GPL-3.0-or-later
#include "remotecatalogmodel.h"

#include "catalogmodel.h"
#include "workshopclient.h"

#include <QJsonArray>
#include <QJsonObject>
#include <QRegularExpression>
#include <QSet>

namespace {
constexpr int BusyRetries = 5;

bool validId(const QString &id) {
  static const QRegularExpression pattern(QStringLiteral("^[0-9]{1,20}$"));
  return pattern.match(id).hasMatch() && id.toULongLong() != 0;
}
}

RemoteCatalogModel::RemoteCatalogModel(WorkshopClient *client, CatalogModel *local,
                                       QObject *parent)
    : QAbstractListModel(parent), m_client(client), m_local(local) {
  if (m_local)
    connect(m_local, &CatalogModel::statsChanged, this,
            &RemoteCatalogModel::onLocalCatalogChanged);
  m_pendingTimer.setInterval(1000);
  connect(&m_pendingTimer, &QTimer::timeout, this, &RemoteCatalogModel::reconcileLocal);
}

int RemoteCatalogModel::rowCount(const QModelIndex &parent) const {
  return parent.isValid() ? 0 : m_rows.size();
}

QVariant RemoteCatalogModel::data(const QModelIndex &index, int role) const {
  if (!index.isValid() || index.row() < 0 || index.row() >= m_rows.size())
    return {};
  const Row &row = m_rows.at(index.row());
  switch (role) {
  case WorkshopIdRole: return row.id;
  case TitleRole: return row.title;
  case ShortDescriptionRole: return row.shortDescription;
  case PreviewUrlRole: return row.previewUrl;
  case KindRole: return row.kind;
  case TagsRole: return row.tags;
  case CreatorIdRole: return row.creatorId;
  case FileSizeRole: return row.fileSize;
  case SubscriptionsRole: return row.subscriptions;
  case ScoreRole: return row.score;
  case TimeUpdatedRole: return row.timeUpdated;
  case SubscribedRole: return m_local && m_local->containsWorkshopId(row.id);
  case SubscriptionPendingRole: return row.pending;
  default: return {};
  }
}

QHash<int, QByteArray> RemoteCatalogModel::roleNames() const {
  return {
      {WorkshopIdRole, "workshopId"},
      {TitleRole, "title"},
      {ShortDescriptionRole, "shortDescription"},
      {PreviewUrlRole, "previewUrl"},
      {KindRole, "kind"},
      {TagsRole, "tags"},
      {CreatorIdRole, "creatorId"},
      {FileSizeRole, "fileSize"},
      {SubscriptionsRole, "subscriptions"},
      {ScoreRole, "score"},
      {TimeUpdatedRole, "timeUpdated"},
      {SubscribedRole, "subscribed"},
      {SubscriptionPendingRole, "subscriptionPending"},
  };
}

void RemoteCatalogModel::setSearchText(const QString &text) {
  if (m_searchText == text)
    return;
  m_searchText = text;
  invalidate();
  emit searchTextChanged();
}

void RemoteCatalogModel::setSort(const QString &sort) {
  if (m_sort == sort)
    return;
  m_sort = sort;
  invalidate();
  emit sortChanged();
}

void RemoteCatalogModel::setTags(const QStringList &tags) {
  if (m_tags == tags)
    return;
  m_tags = tags;
  invalidate();
  emit tagsChanged();
}

void RemoteCatalogModel::invalidate() {
  ++m_generation;
  m_previewQueue.clear();
  setLoadingMore(false);
}

void RemoteCatalogModel::refresh() {
  if (!m_client)
    return;
  invalidate();
  m_page = 0;
  if (!m_rows.isEmpty()) {
    beginResetModel();
    m_rows.clear();
    m_index.clear();
    endResetModel();
    emit countChanged();
  }
  setHasMore(false);
  setErrorMessage({});
  setNotice({});
  setState(Loading);
  requestPage(1);
}

void RemoteCatalogModel::loadMore() {
  if (!m_client || !m_hasMore || m_loadingMore || m_state != Ready)
    return;
  setLoadingMore(true);
  requestPage(m_page + 1);
}

void RemoteCatalogModel::requestPage(int page) {
  const quint64 generation = m_generation;
  QJsonArray tags;
  for (const QString &tag : std::as_const(m_tags))
    tags.push_back(tag);
  const QJsonObject params{{QStringLiteral("search"), m_searchText},
                           {QStringLiteral("tags"), tags},
                           {QStringLiteral("sort"), m_sort},
                           {QStringLiteral("page"), page}};
  m_client->call(QStringLiteral("workshop.query"), params, this,
                 [this, generation, page](const WorkshopClient::Reply &reply) {
                   if (generation != m_generation)
                     return;  // superseded by a newer search/sort/tag/refresh
                   if (reply.ok)
                     handlePage(reply.result, page);
                   else
                     handlePageError(reply.error,
                                     WorkshopClient::describeError(reply.error, reply.detail),
                                     page);
                 },
                 BusyRetries);
}

void RemoteCatalogModel::handlePage(const QJsonObject &result, int page) {
  const QJsonArray items = result.value(QStringLiteral("items")).toArray();
  QVector<Row> fresh;
  QSet<QString> seen;
  for (const Row &row : std::as_const(m_rows))
    seen.insert(row.id);
  for (const QJsonValue &value : items) {
    const QJsonObject object = value.toObject();
    Row row;
    row.id = object.value(QStringLiteral("id")).toString();
    if (!validId(row.id) || seen.contains(row.id))
      continue;
    if (m_rows.size() + fresh.size() >= MaxRows)
      break;
    seen.insert(row.id);
    row.title = object.value(QStringLiteral("title")).toString();
    row.shortDescription = object.value(QStringLiteral("short_description")).toString();
    row.kind = object.value(QStringLiteral("kind")).toString();
    row.creatorId = object.value(QStringLiteral("creator_id")).toString();
    row.previewRemote = object.value(QStringLiteral("preview_url")).toString();
    const QString file = object.value(QStringLiteral("preview_file")).toString();
    if (!file.isEmpty())
      row.previewUrl = QUrl::fromLocalFile(file);
    for (const QJsonValue &tag : object.value(QStringLiteral("tags")).toArray())
      row.tags.push_back(tag.toString());
    row.fileSize = static_cast<qint64>(object.value(QStringLiteral("file_size")).toDouble());
    row.subscriptions = static_cast<qint64>(object.value(QStringLiteral("subscriptions")).toDouble());
    row.score = object.value(QStringLiteral("score")).toDouble();
    row.timeUpdated = static_cast<qint64>(object.value(QStringLiteral("time_updated")).toDouble());
    row.pending = m_changes.contains(row.id);
    fresh.push_back(std::move(row));
  }
  m_page = page;
  QStringList needPreview;
  if (!fresh.isEmpty()) {
    beginInsertRows({}, m_rows.size(), m_rows.size() + fresh.size() - 1);
    for (Row &row : fresh) {
      if (row.previewUrl.isEmpty() && !row.previewRemote.isEmpty())
        needPreview.push_back(row.id);
      m_rows.push_back(std::move(row));
    }
    rebuildIndex();
    endInsertRows();
    emit countChanged();
  }
  setLoadingMore(false);
  setHasMore(result.value(QStringLiteral("has_more")).toBool() && m_rows.size() < MaxRows);
  setErrorMessage({});
  setState(m_rows.isEmpty() ? Empty : Ready);
  requestPreviews(needPreview);
}

void RemoteCatalogModel::handlePageError(const QString &code, const QString &message, int page) {
  setLoadingMore(false);
  setErrorMessage(message);
  if (page > 1 && !m_rows.isEmpty() && code != QLatin1String("signed_out")) {
    // Keep what is already on screen; the page can offer a retry of loadMore.
    return;
  }
  if (code == QLatin1String("offline"))
    setState(Offline);
  else if (code == QLatin1String("signed_out"))
    setState(SignedOut);
  else if (code == QLatin1String("rate_limited"))
    setState(RateLimited);
  else
    setState(Failed);
}

void RemoteCatalogModel::requestPreviews(const QStringList &ids) {
  m_previewQueue.append(ids);
  pumpPreviews();
}

// Sends queued thumbnail requests, a few small batches at a time. An id the
// daemon did not answer for (slow image, busy worker) goes back on the queue
// for a bounded number of attempts.
void RemoteCatalogModel::pumpPreviews() {
  if (!m_client)
    return;
  while (m_previewCalls < PreviewCallsInFlight && !m_previewQueue.isEmpty()) {
    QJsonArray batch;
    QStringList asked;
    while (!m_previewQueue.isEmpty() && asked.size() < PreviewBatch) {
      const QString id = m_previewQueue.takeFirst();
      const int row = rowOf(id);
      if (row < 0 || !m_rows.at(row).previewUrl.isEmpty() || asked.contains(id))
        continue;
      ++m_rows[row].previewAttempts;
      asked.push_back(id);
      batch.push_back(QJsonObject{{QStringLiteral("id"), id},
                                  {QStringLiteral("url"), m_rows.at(row).previewRemote}});
    }
    if (batch.isEmpty())
      break;
    ++m_previewCalls;
    m_client->call(QStringLiteral("workshop.previews"),
                   QJsonObject{{QStringLiteral("items"), batch}}, this,
                   [this, asked](const WorkshopClient::Reply &reply) {
                     --m_previewCalls;
                     const QJsonObject files =
                         reply.ok ? reply.result.value(QStringLiteral("files")).toObject()
                                  : QJsonObject{};
                     for (const QString &id : asked) {
                       const int row = rowOf(id);
                       if (row < 0)
                         continue;
                       const QString path = files.value(id).toString();
                       if (!path.isEmpty()) {
                         m_rows[row].previewUrl = QUrl::fromLocalFile(path);
                         emit dataChanged(index(row), index(row), {PreviewUrlRole});
                       } else if (m_rows.at(row).previewAttempts < PreviewAttempts) {
                         m_previewQueue.push_back(id);
                       }  // else: the card keeps its icon fallback
                     }
                     pumpPreviews();
                   },
                   BusyRetries);
  }
}

void RemoteCatalogModel::subscribe(const QString &workshopId) { mutate(workshopId, false); }
void RemoteCatalogModel::unsubscribe(const QString &workshopId) { mutate(workshopId, true); }

void RemoteCatalogModel::mutate(const QString &workshopId, bool unsubscribe) {
  if (!m_client || !validId(workshopId) || m_changes.contains(workshopId))
    return;
  setNotice({});
  PendingChange change;
  change.unsubscribe = unsubscribe;
  change.age.start();
  m_changes.insert(workshopId, change);
  emit pendingIdsChanged();
  const int row = rowOf(workshopId);
  if (row >= 0) {
    m_rows[row].pending = true;
    emit dataChanged(index(row), index(row), {SubscriptionPendingRole});
  }
  m_client->call(unsubscribe ? QStringLiteral("workshop.unsubscribe")
                             : QStringLiteral("workshop.subscribe"),
                 QJsonObject{{QStringLiteral("id"), workshopId}}, this,
                 [this, workshopId](const WorkshopClient::Reply &reply) {
                   if (!reply.ok) {
                     clearPending(workshopId,
                                  WorkshopClient::describeError(reply.error, reply.detail));
                     return;
                   }
                   auto it = m_changes.find(workshopId);
                   if (it == m_changes.end())
                     return;
                   it->accepted = true;
                   it->age.restart();
                   m_pendingTimer.start();
                   reconcileLocal();
                 },
                 BusyRetries);
}

QStringList RemoteCatalogModel::pendingIds() const {
  QStringList ids = m_changes.keys();
  ids.sort();
  return ids;
}

void RemoteCatalogModel::clearPending(const QString &id, const QString &note) {
  if (m_changes.remove(id) > 0)
    emit pendingIdsChanged();
  const int row = rowOf(id);
  if (row >= 0) {
    m_rows[row].pending = false;
    emit dataChanged(index(row), index(row), {SubscriptionPendingRole, SubscribedRole});
  }
  if (!note.isEmpty())
    setNotice(note, id);
  if (m_changes.isEmpty())
    m_pendingTimer.stop();
}

void RemoteCatalogModel::onLocalCatalogChanged() {
  if (!m_rows.isEmpty())
    emit dataChanged(index(0), index(m_rows.size() - 1), {SubscribedRole});
  reconcileLocal();
}

void RemoteCatalogModel::reconcileLocal() {
  const QStringList ids = m_changes.keys();
  for (const QString &id : ids) {
    const PendingChange change = m_changes.value(id);
    if (!change.accepted)
      continue;
    const bool present = m_local && m_local->containsWorkshopId(id);
    if (present != change.unsubscribe) {
      clearPending(id);
    } else if (change.age.hasExpired(m_pendingTimeoutMs)) {
      clearPending(id, change.unsubscribe
                           ? tr("Steam has not removed the item yet. Is the Steam client running?")
                           : tr("Steam has not started the download — is the Steam client running?"));
    }
  }
}

void RemoteCatalogModel::details(const QString &workshopId) {
  if (!m_client || !validId(workshopId))
    return;
  m_client->call(QStringLiteral("workshop.details"),
                 QJsonObject{{QStringLiteral("id"), workshopId}}, this,
                 [this, workshopId](const WorkshopClient::Reply &reply) {
                   if (reply.ok)
                     emit detailsReady(workshopId,
                                       reply.result.value(QStringLiteral("description")).toString());
                   else
                     emit detailsFailed(workshopId,
                                        WorkshopClient::describeError(reply.error, reply.detail));
                 },
                 BusyRetries);
}

QVariantMap RemoteCatalogModel::itemById(const QString &workshopId) const {
  const int row = rowOf(workshopId);
  if (row < 0)
    return {};
  const QModelIndex idx = index(row);
  QVariantMap map;
  const auto names = roleNames();
  for (auto it = names.begin(); it != names.end(); ++it)
    map.insert(QString::fromUtf8(it.value()), data(idx, it.key()));
  return map;
}

int RemoteCatalogModel::rowOf(const QString &id) const {
  return m_index.value(id, -1);
}

void RemoteCatalogModel::rebuildIndex() {
  m_index.clear();
  for (int i = 0; i < m_rows.size(); ++i)
    m_index.insert(m_rows.at(i).id, i);
}

void RemoteCatalogModel::setState(State state) {
  if (m_state == state)
    return;
  m_state = state;
  emit stateChanged();
}

void RemoteCatalogModel::setErrorMessage(const QString &message) {
  if (m_errorMessage == message)
    return;
  m_errorMessage = message;
  emit errorMessageChanged();
}

void RemoteCatalogModel::setNotice(const QString &message, const QString &id) {
  const QString noticeId = message.isEmpty() ? QString() : id;
  if (m_notice == message && m_noticeId == noticeId)
    return;
  m_notice = message;
  m_noticeId = noticeId;
  emit noticeChanged();
}

void RemoteCatalogModel::setHasMore(bool value) {
  if (m_hasMore == value)
    return;
  m_hasMore = value;
  emit hasMoreChanged();
}

void RemoteCatalogModel::setLoadingMore(bool value) {
  if (m_loadingMore == value)
    return;
  m_loadingMore = value;
  emit loadingMoreChanged();
}
