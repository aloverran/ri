import { createSignal, For } from 'solid-js';
import { marked } from 'marked';
import { Message, ContentBlock } from '../types';

interface MessageViewProps {
  message: Message;
}

export default function MessageView(props: MessageViewProps) {
  const isUser = () => props.message.role === 'user';
  const isAssistant = () => props.message.role === 'assistant';

  return (
    <div class={`message ${props.message.role}`}>
      <div class="message-header">
        <span class="message-role">{props.message.role}</span>
        {props.message.provenance?.ts && (
          <span class="message-timestamp">
            {new Date(props.message.provenance.ts).toLocaleTimeString()}
          </span>
        )}
      </div>
      <div class="message-content">
        <For each={props.message.content}>
          {(block) => <ContentBlockView block={block} />}
        </For>
      </div>
    </div>
  );
}

interface ContentBlockViewProps {
  block: ContentBlock;
}

function ContentBlockView(props: ContentBlockViewProps) {
  const [thinkingExpanded, setThinkingExpanded] = createSignal(false);
  const [toolExpanded, setToolExpanded] = createSignal(false);

  switch (props.block.type) {
    case 'text':
      return (
        <div 
          class="content-text" 
          innerHTML={marked(props.block.text)}
        />
      );

    case 'thinking':
      return (
        <div class="content-thinking">
          <button 
            class="thinking-toggle"
            onclick={() => setThinkingExpanded(!thinkingExpanded())}
          >
            <span class={`expand-icon ${thinkingExpanded() ? 'expanded' : ''}`}>▶</span>
            Thinking...
          </button>
          {thinkingExpanded() && (
            <div class="thinking-content">
              <pre>{props.block.thinking}</pre>
            </div>
          )}
        </div>
      );

    case 'tool_use':
      return (
        <div class="content-tool-use">
          <button 
            class="tool-toggle"
            onclick={() => setToolExpanded(!toolExpanded())}
          >
            <span class={`expand-icon ${toolExpanded() ? 'expanded' : ''}`}>▶</span>
            🔧 {props.block.name}
          </button>
          {toolExpanded() && (
            <div class="tool-content">
              <pre>{JSON.stringify(props.block.input, null, 2)}</pre>
            </div>
          )}
        </div>
      );

    case 'tool_result':
      return (
        <div class={`content-tool-result ${props.block.is_error ? 'error' : ''}`}>
          <div class="tool-result-header">
            {props.block.is_error ? '❌' : '✅'} Tool Result
          </div>
          <div class="tool-result-content">
            <For each={props.block.content}>
              {(block) => <ContentBlockView block={block} />}
            </For>
          </div>
        </div>
      );

    case 'image':
      return (
        <div class="content-image">
          <img 
            src={`data:${props.block.mediaType};base64,${props.block.data}`}
            alt="Content image"
          />
        </div>
      );

    default:
      return <div class="content-unknown">Unknown content type</div>;
  }
}