import type { CommandInfo, SkillInfo } from '../../api';
import { useT } from '../../settings';
import {
  FRONTEND_COMMANDS,
  buildGoalMenuItems,
  buildSlashMenuItems,
  type SlashMenuItem,
} from '../../lib/slashCommands';

export function SlashMenu({
  skills,
  userCommands,
  query,
  skillsOnly,
  goalContext,
  index,
  onHover,
  onSelect,
}: {
  skills: SkillInfo[];
  userCommands: CommandInfo[];
  query: string;
  skillsOnly: boolean;
  goalContext?: { token: string; firstToken: boolean } | null;
  index: number;
  onHover: (index: number) => void;
  onSelect: (item: SlashMenuItem) => void;
}) {
  const t = useT();
  const items = goalContext
    ? buildGoalMenuItems(goalContext.token, goalContext.firstToken, t as (k: string) => string)
    : buildSlashMenuItems(
        FRONTEND_COMMANDS,
        skills,
        userCommands,
        query,
        t as (k: string) => string,
        skillsOnly,
      );
  return (
    <div class="slash-popover">
      {items.map((item, i) => (
        <button
          key={`${item.kind}:${item.name}`}
          class={'slash-row' + (i === index ? ' active' : '')}
          onMouseDown={(e) => { e.preventDefault(); e.stopPropagation(); onSelect(item); }}
          onMouseEnter={() => onHover(i)}
          type="button"
          title={item.description || ''}
        >
          <span class={'slash-name' + (item.kind === 'skill' ? ' notranslate' : '')} translate={item.kind === 'skill' ? false : undefined}>
            {item.kind === 'goal' ? item.name : `/${item.name}`}
          </span>
          {item.description && (
            <span class={'slash-desc' + (item.kind === 'skill' ? ' notranslate' : '')} translate={item.kind === 'skill' ? false : undefined}>
              {item.description}
            </span>
          )}
        </button>
      ))}
    </div>
  );
}
