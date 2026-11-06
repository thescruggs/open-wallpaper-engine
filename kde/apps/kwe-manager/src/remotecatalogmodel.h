// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

#include <QAbstractListModel>
#include <QElapsedTimer>
#include <QHash>
#include <QPointer>
#include <QQmlEngine>
#include <QStringList>
#include <QTimer>
#include <QUrl>
#include <QVariantMap>
#include <QVector>

class CatalogModel;
class WorkshopClient;

// The Steam Workshop "Browse" list, fed page by page from workshop.query.
// Rows are remote metadata only; nothing here is installed content. The
// "subscribed" role is computed against the local catalog (CatalogModel),
// which is the single source of truth for what Steam has actually
// downloaded — a subscribe request stays "pending" until the local catalog
// lists the item.
class RemoteCatalogModel : public QAbstractListModel {
  Q_OBJECT
  QML_ELEMENT
  QML_UNCREATABLE("RemoteCatalogModel is created by the manager")
  Q_PROPERTY(State state READ state NOTIFY stateChanged)
  Q_PROPERTY(QString errorMessage READ errorMessage NOTIFY errorMessageChanged)
  Q_PROPERTY(QString notice READ notice NOTIFY noticeChanged)
  Q_PROPERTY(QString searchText READ searchText WRITE setSearchText NOTIFY searchTextChanged)
  Q_PROPERTY(QString sort READ sort WRITE setSort NOTIFY sortChanged)
  Q_PROPERTY(QStringList tags READ tags WRITE setTags NOTIFY tagsChanged)
  Q_PROPERTY(bool hasMore READ hasMore NOTIFY hasMoreChanged)
  Q_PROPERTY(bool loadingMore READ loadingMore NOTIFY loadingMoreChanged)
  Q_PROPERTY(int count READ rowCount NOTIFY countChanged)
  // Ids with a subscribe/unsubscribe request in flight or awaiting the local
  // catalog, whether or not they are a Browse row: the details pane of the
  // Subscribed view follows its own unsubscribe through this list (W5).
  Q_PROPERTY(QStringList pendingIds READ pendingIds NOTIFY pendingIdsChanged)

public:
  enum State { Idle, Loading, Ready, Empty, Offline, SignedOut, RateLimited, Failed };
  Q_ENUM(State)

  enum Role {
    WorkshopIdRole = Qt::UserRole + 1,
    TitleRole,
    ShortDescriptionRole,
    PreviewUrlRole,
    KindRole,
    TagsRole,
    CreatorIdRole,
    FileSizeRole,
    SubscriptionsRole,
    ScoreRole,
    TimeUpdatedRole,
    SubscribedRole,
    SubscriptionPendingRole,
  };

  static constexpr int MaxRows = 2000;
  // Small batches, a few at a time: thumbnails appear as they arrive and a
  // slow image never holds back a whole page.
  static constexpr int PreviewBatch = 8;
  static constexpr int PreviewCallsInFlight = 3;
  static constexpr int PreviewAttempts = 3;

  RemoteCatalogModel(WorkshopClient *client, CatalogModel *local, QObject *parent = nullptr);

  int rowCount(const QModelIndex &parent = {}) const override;
  QVariant data(const QModelIndex &index, int role) const override;
  QHash<int, QByteArray> roleNames() const override;

  State state() const { return m_state; }
  QString errorMessage() const { return m_errorMessage; }
  QString notice() const { return m_notice; }
  QString searchText() const { return m_searchText; }
  QString sort() const { return m_sort; }
  QStringList tags() const { return m_tags; }
  bool hasMore() const { return m_hasMore; }
  bool loadingMore() const { return m_loadingMore; }
  QStringList pendingIds() const;
  void setSearchText(const QString &text);
  void setSort(const QString &sort);
  void setTags(const QStringList &tags);

  Q_INVOKABLE void refresh();
  Q_INVOKABLE void loadMore();
  Q_INVOKABLE void subscribe(const QString &workshopId);
  Q_INVOKABLE void unsubscribe(const QString &workshopId);
  Q_INVOKABLE void details(const QString &workshopId);
  // Current row values by id (empty map when the row is gone); the detail
  // sheet re-reads it on dataChanged to follow subscription state.
  Q_INVOKABLE QVariantMap itemById(const QString &workshopId) const;

  // Test hook: how long a subscription stays pending after Steam accepted it.
  void setPendingTimeout(int milliseconds) { m_pendingTimeoutMs = milliseconds; }

signals:
  void stateChanged();
  void errorMessageChanged();
  void noticeChanged();
  void searchTextChanged();
  void sortChanged();
  void tagsChanged();
  void hasMoreChanged();
  void loadingMoreChanged();
  void countChanged();
  void pendingIdsChanged();
  void detailsReady(const QString &workshopId, const QString &description);
  void detailsFailed(const QString &workshopId, const QString &message);

private:
  struct Row {
    QString id, title, shortDescription, kind, creatorId, previewRemote;
    QUrl previewUrl;
    QStringList tags;
    qint64 fileSize = 0;
    qint64 subscriptions = 0;
    double score = 0;
    qint64 timeUpdated = 0;
    bool pending = false;
    int previewAttempts = 0;
  };
  struct PendingChange {
    bool unsubscribe = false;
    bool accepted = false;
    QElapsedTimer age;
  };

  void invalidate();  // supersede in-flight query answers
  void requestPage(int page);
  void handlePage(const QJsonObject &result, int page);
  void handlePageError(const QString &code, const QString &message, int page);
  void requestPreviews(const QStringList &ids);
  void pumpPreviews();
  void mutate(const QString &workshopId, bool unsubscribe);
  void onLocalCatalogChanged();
  void reconcileLocal();
  void clearPending(const QString &id, const QString &note = {});
  int rowOf(const QString &id) const;
  void setState(State state);
  void setErrorMessage(const QString &message);
  void setNotice(const QString &message);
  void setHasMore(bool value);
  void setLoadingMore(bool value);
  void rebuildIndex();

  QPointer<WorkshopClient> m_client;
  QPointer<CatalogModel> m_local;
  QVector<Row> m_rows;
  QStringList m_previewQueue;
  int m_previewCalls = 0;
  QHash<QString, int> m_index;
  QHash<QString, PendingChange> m_changes;
  QTimer m_pendingTimer;
  State m_state = Idle;
  QString m_errorMessage;
  QString m_notice;
  QString m_searchText;
  QString m_sort = QStringLiteral("trend_week");
  QStringList m_tags;
  bool m_hasMore = false;
  bool m_loadingMore = false;
  int m_page = 0;
  int m_pendingTimeoutMs = 120000;
  quint64 m_generation = 0;
};
