import { createSignal, createResource, For } from 'solid-js';
import { getSessions, createSession } from '../api';
import { SessionSummary } from '../types';

interface SessionListProps {
  onSelect: (id: string) => void;
}

export default function SessionList(props: SessionListProps) {
  const [sessions, { refetch }] = createResource<SessionSummary[]>(getSessions);
  const [newSessionName, setNewSessionName] = createSignal('');
  const [newSessionCwd, setNewSessionCwd] = createSignal('/Users/john/Projects/ri2');
  const [creating, setCreating] = createSignal(false);

  const handleCreateSession = async (e: Event) => {
    e.preventDefault();
    const name = newSessionName().trim();
    const cwd = newSessionCwd().trim();
    
    if (!name || !cwd) return;

    setCreating(true);
    try {
      const result = await createSession(name, cwd);
      setNewSessionName('');
      refetch();
      props.onSelect(result.id);
    } catch (error) {
      console.error('Failed to create session:', error);
    } finally {
      setCreating(false);
    }
  };

  return (
    <div class="session-list">
      <header class="session-list-header">
        <h1>RI Chat Sessions</h1>
      </header>

      <div class="new-session">
        <h2>New Session</h2>
        <form onSubmit={handleCreateSession}>
          <div class="form-group">
            <label for="session-name">Session Name</label>
            <input
              id="session-name"
              type="text"
              placeholder="Enter session name..."
              value={newSessionName()}
              onInput={(e) => setNewSessionName(e.currentTarget.value)}
              disabled={creating()}
            />
          </div>
          <div class="form-group">
            <label for="session-cwd">Working Directory</label>
            <input
              id="session-cwd"
              type="text"
              placeholder="/path/to/project"
              value={newSessionCwd()}
              onInput={(e) => setNewSessionCwd(e.currentTarget.value)}
              disabled={creating()}
            />
          </div>
          <button 
            type="submit" 
            disabled={creating() || !newSessionName().trim() || !newSessionCwd().trim()}
          >
            {creating() ? 'Creating...' : 'Create Session'}
          </button>
        </form>
      </div>

      <div class="sessions">
        <h2>Existing Sessions</h2>
        {sessions.loading && <div class="loading">Loading sessions...</div>}
        {sessions.error && <div class="error">Failed to load sessions</div>}
        {sessions() && sessions()!.length === 0 && (
          <div class="empty">No sessions yet. Create one above.</div>
        )}
        <For each={sessions()}>
          {(session) => (
            <div 
              class="session-item"
              onclick={() => props.onSelect(session.id)}
            >
              <div class="session-name">{session.name}</div>
              <div class="session-meta">
                <span class="session-cwd">{session.cwd}</span>
                <span class="session-count">{session.message_count} messages</span>
                <span class="session-timestamp">
                  {new Date(session.ts).toLocaleString()}
                </span>
              </div>
            </div>
          )}
        </For>
      </div>
    </div>
  );
}