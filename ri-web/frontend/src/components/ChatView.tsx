import { createSignal, createResource, createEffect, onCleanup, For } from 'solid-js';
import { getSession, sendMessage, cancelSession, connectSSE } from '../api';
import { SessionDetail, Message, ContentBlock } from '../types';
import MessageView from './MessageView';

interface ChatViewProps {
  sessionId: string;
  onBack: () => void;
}

export default function ChatView(props: ChatViewProps) {
  const [session, { refetch }] = createResource(() => props.sessionId, getSession);
  const [messageText, setMessageText] = createSignal('');
  const [sending, setSending] = createSignal(false);

  // Streaming state
  const [streamingText, setStreamingText] = createSignal('');
  const [streamingThinking, setStreamingThinking] = createSignal('');
  const [isStreaming, setIsStreaming] = createSignal(false);
  
  let messagesContainer: HTMLDivElement;
  let eventSource: EventSource | null = null;

  // Auto-scroll to bottom
  const scrollToBottom = () => {
    if (messagesContainer) {
      messagesContainer.scrollTop = messagesContainer.scrollHeight;
    }
  };

  // Set up SSE connection
  createEffect(() => {
    if (props.sessionId) {
      eventSource?.close();
      
      eventSource = connectSSE(props.sessionId, {
        text_start: () => {
          setIsStreaming(true);
          setStreamingText('');
        },
        text_delta: (data) => {
          setStreamingText(prev => prev + data.delta);
          scrollToBottom();
        },
        text_end: () => {
          // Keep text until message_complete
        },
        thinking_start: () => {
          setStreamingThinking('');
        },
        thinking_delta: (data) => {
          setStreamingThinking(prev => prev + data.delta);
          scrollToBottom();
        },
        thinking_end: () => {
          // Keep thinking until message_complete
        },
        message_complete: (data) => {
          setIsStreaming(false);
          setStreamingText('');
          setStreamingThinking('');
          refetch();
          scrollToBottom();
        },
        done: () => {
          setIsStreaming(false);
          refetch();
        },
        error: (error) => {
          console.error('SSE Error:', error);
          setIsStreaming(false);
        },
        resync: () => {
          refetch();
        },
      });
    }
  });

  onCleanup(() => {
    eventSource?.close();
  });

  // Auto-scroll when messages change
  createEffect(() => {
    if (session()) {
      setTimeout(scrollToBottom, 100);
    }
  });

  const handleSendMessage = async (e: Event) => {
    e.preventDefault();
    const text = messageText().trim();
    if (!text || sending()) return;

    setSending(true);
    try {
      await sendMessage(props.sessionId, text);
      setMessageText('');
      refetch();
    } catch (error) {
      console.error('Failed to send message:', error);
    } finally {
      setSending(false);
    }
  };

  const handleCancel = async () => {
    try {
      await cancelSession(props.sessionId);
    } catch (error) {
      console.error('Failed to cancel session:', error);
    }
  };

  const isRunning = () => session()?.status === 'running' || isStreaming();

  return (
    <div class="chat-view">
      <header class="chat-header">
        <button class="back-button" onclick={props.onBack}>
          ← Back
        </button>
        <div class="session-info">
          <h1>{session()?.name || 'Loading...'}</h1>
          {session() && (
            <div class="session-meta">
              <span class="session-cwd">{session()!.cwd}</span>
              <span class={`session-status ${session()!.status}`}>
                {session()!.status}
              </span>
            </div>
          )}
        </div>
        {isRunning() && (
          <button class="cancel-button" onclick={handleCancel}>
            Cancel
          </button>
        )}
      </header>

      <div class="messages-container" ref={messagesContainer!}>
        {session.loading && <div class="loading">Loading session...</div>}
        {session.error && <div class="error">Failed to load session</div>}
        
        <For each={session()?.messages}>
          {(message) => <MessageView message={message} />}
        </For>

        {/* Streaming preview */}
        {(streamingText() || streamingThinking()) && (
          <div class="message assistant streaming">
            <div class="message-header">
              <span class="message-role">assistant</span>
              <span class="streaming-indicator">●</span>
            </div>
            <div class="message-content">
              {streamingThinking() && (
                <div class="content-thinking">
                  <div class="thinking-toggle expanded">
                    <span class="expand-icon expanded">▼</span>
                    Thinking...
                  </div>
                  <div class="thinking-content">
                    <pre>{streamingThinking()}<span class="cursor">█</span></pre>
                  </div>
                </div>
              )}
              {streamingText() && (
                <div class="content-text">
                  {streamingText()}<span class="cursor">█</span>
                </div>
              )}
            </div>
          </div>
        )}
      </div>

      <form class="message-form" onSubmit={handleSendMessage}>
        <div class="message-input-container">
          <textarea
            class="message-input"
            placeholder="Type your message..."
            value={messageText()}
            onInput={(e) => setMessageText(e.currentTarget.value)}
            disabled={sending() || isRunning()}
            rows="3"
            onKeyDown={(e) => {
              if (e.key === 'Enter' && !e.shiftKey) {
                e.preventDefault();
                handleSendMessage(e);
              }
            }}
          />
          <button 
            type="submit" 
            class="send-button"
            disabled={sending() || isRunning() || !messageText().trim()}
          >
            {sending() ? 'Sending...' : 'Send'}
          </button>
        </div>
      </form>
    </div>
  );
}