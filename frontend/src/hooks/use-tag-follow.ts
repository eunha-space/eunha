import { useEffect, useState } from 'react'
import { toast } from 'sonner'
import { getTag, setTagFollow } from '../api.ts'

export function useTagFollow(name: string, token: string) {
  const [following, setFollowing] = useState<boolean>()
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    if (!token || !name) return
    let active = true
    getTag(name, token).then((tag) => {
      if (active) setFollowing(tag.following ?? false)
    }).catch(() => {
      if (active) toast.error('Could not load hashtag')
    })
    return () => { active = false }
  }, [name, token])

  const toggle = async () => {
    if (following === undefined || busy) return
    setBusy(true)
    try {
      const tag = await setTagFollow(name, token, !following)
      setFollowing(tag.following ?? false)
    } catch {
      toast.error('Could not update hashtag follow')
    } finally {
      setBusy(false)
    }
  }

  return { following, disabled: busy || following === undefined, toggle }
}
