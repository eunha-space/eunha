self.addEventListener('push', (event) => {
  if (!event.data) return
  let payload
  try {
    payload = event.data.json()
  } catch {
    return
  }
  if (typeof payload.title !== 'string') return
  event.waitUntil(self.registration.showNotification(payload.title, {
    body: typeof payload.body === 'string' ? payload.body : '',
  }))
})

self.addEventListener('notificationclick', (event) => {
  event.notification.close()
  event.waitUntil(self.clients.openWindow('/notifications'))
})
